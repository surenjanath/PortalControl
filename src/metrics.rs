//! Resource metrics of the host, the manager itself and each supervised
//! process tree, sampled every few seconds into small bounded rings.

use crate::state::AppState;
use crate::util::now_secs;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// Samples kept per series (15 minutes at the 5 s interval).
const SAMPLES_KEPT: usize = 180;

/// CPU is a percentage of the whole machine (all cores = 100); memory is bytes.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcSample {
    pub t: u64,
    pub cpu: f32,
    pub mem: u64,
    pub procs: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostSample {
    pub t: u64,
    pub cpu: f32,
    pub mem_used: u64,
    pub manager_cpu: f32,
    pub manager_mem: u64,
}

#[derive(Default)]
pub struct MetricsStore {
    pub host: VecDeque<HostSample>,
    pub targets: HashMap<i64, VecDeque<ProcSample>>,
    pub mem_total: u64,
    pub cpus: usize,
    pub host_name: Option<String>,
    pub os_version: Option<String>,
}

impl MetricsStore {
    pub fn latest(&self, target_id: i64) -> Option<ProcSample> {
        self.targets.get(&target_id).and_then(|r| r.back().cloned())
    }
}

/// One row of the process table: pid, parent, start time, cpu %, memory.
#[derive(Debug, Clone, Copy)]
pub struct ProcRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub start: u64,
    pub cpu: f32,
    pub mem: u64,
}

/// Sum CPU and memory over `root` and all of its descendants. A child must
/// not be older than its parent, which guards against recycled process ids.
pub fn tree_usage(root: u32, table: &[ProcRow]) -> (f32, u64, u32) {
    let by_pid: HashMap<u32, &ProcRow> = table.iter().map(|r| (r.pid, r)).collect();
    let mut children: HashMap<u32, Vec<&ProcRow>> = HashMap::new();
    for row in table {
        if let Some(parent) = row.parent {
            children.entry(parent).or_default().push(row);
        }
    }
    let Some(root_row) = by_pid.get(&root) else { return (0.0, 0, 0) };
    let (mut cpu, mut mem, mut count) = (0.0f32, 0u64, 0u32);
    let mut stack = vec![*root_row];
    let mut seen = std::collections::HashSet::new();
    while let Some(row) = stack.pop() {
        if !seen.insert(row.pid) {
            continue;
        }
        cpu += row.cpu;
        mem += row.mem;
        count += 1;
        for child in children.get(&row.pid).into_iter().flatten() {
            if child.start >= row.start {
                stack.push(child);
            }
        }
    }
    (cpu, mem, count)
}

fn push_bounded<T>(ring: &mut VecDeque<T>, sample: T) {
    ring.push_back(sample);
    while ring.len() > SAMPLES_KEPT {
        ring.pop_front();
    }
}

struct Snapshot {
    table: Vec<ProcRow>,
    host_cpu: f32,
    mem_used: u64,
    manager_cpu: f32,
    manager_mem: u64,
}

/// Refresh `sys` and read what is needed. The full process table is only
/// collected when something is running, which keeps the idle cost negligible.
fn sample(sys: &mut System, own: Pid, want_table: bool) -> Snapshot {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
    if want_table {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    } else {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[own]), true, kind);
    }
    let table = if want_table {
        sys.processes()
            .iter()
            .map(|(pid, p)| ProcRow {
                pid: pid.as_u32(),
                parent: p.parent().map(|x| x.as_u32()),
                start: p.start_time(),
                cpu: p.cpu_usage(),
                mem: p.memory(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let (manager_cpu, manager_mem) = sys
        .process(own)
        .map(|p| (p.cpu_usage(), p.memory()))
        .unwrap_or((0.0, 0));
    Snapshot {
        table,
        host_cpu: sys.global_cpu_usage(),
        mem_used: sys.used_memory(),
        manager_cpu,
        manager_mem,
    }
}

pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let own = Pid::from_u32(std::process::id());
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let cpus = sys.cpus().len().max(1);
        if let Ok(mut store) = state.metrics.lock() {
            store.mem_total = sys.total_memory();
            store.cpus = cpus;
            store.host_name = System::host_name();
            store.os_version = System::long_os_version();
        }

        loop {
            tokio::time::sleep(SAMPLE_INTERVAL).await;
            let roots: Vec<(i64, u32)> = state
                .running
                .lock()
                .await
                .iter()
                // Negative keys are one-off commands, not supervised targets.
                .filter(|(id, _)| **id > 0)
                .filter_map(|(id, h)| h.pid.map(|p| (*id, p)))
                .collect();
            let want_table = !roots.is_empty();

            // Reading the process table is blocking work.
            let joined = tokio::task::spawn_blocking(move || {
                let snap = sample(&mut sys, own, want_table);
                (sys, snap)
            })
            .await;
            let snap = match joined {
                Ok((s, snap)) => {
                    sys = s;
                    snap
                }
                Err(_) => {
                    sys = System::new();
                    continue;
                }
            };

            let t = now_secs();
            let per_core = cpus as f32;
            if let Ok(mut store) = state.metrics.lock() {
                push_bounded(
                    &mut store.host,
                    HostSample {
                        t,
                        cpu: snap.host_cpu,
                        mem_used: snap.mem_used,
                        manager_cpu: snap.manager_cpu / per_core,
                        manager_mem: snap.manager_mem,
                    },
                );
                for (target_id, pid) in &roots {
                    let (cpu, mem, procs) = tree_usage(*pid, &snap.table);
                    push_bounded(
                        store.targets.entry(*target_id).or_default(),
                        ProcSample { t, cpu: cpu / per_core, mem, procs },
                    );
                }
                // Forget series of targets that have been idle for a while.
                let horizon = t.saturating_sub(SAMPLE_INTERVAL.as_secs() * SAMPLES_KEPT as u64);
                store
                    .targets
                    .retain(|_, ring| ring.back().map(|s| s.t >= horizon).unwrap_or(false));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, parent: Option<u32>, start: u64, cpu: f32, mem: u64) -> ProcRow {
        ProcRow { pid, parent, start, cpu, mem }
    }

    #[test]
    fn usage_is_summed_over_the_process_tree() {
        let table = vec![
            row(10, Some(1), 100, 1.0, 1_000),  // cmd.exe (root)
            row(11, Some(10), 101, 20.0, 5_000), // python
            row(12, Some(11), 102, 5.0, 2_000),  // worker
            row(13, Some(11), 102, 0.5, 500),
            row(20, Some(1), 90, 50.0, 9_000),   // unrelated
            row(30, Some(10), 50, 9.0, 9_999),   // older than its "parent": recycled pid
        ];
        let (cpu, mem, procs) = tree_usage(10, &table);
        assert!((cpu - 26.5).abs() < 0.001);
        assert_eq!(mem, 8_500);
        assert_eq!(procs, 4);
        assert_eq!(tree_usage(999, &table), (0.0, 0, 0));
    }

    #[test]
    fn cycles_in_the_process_table_terminate() {
        let table = vec![row(1, Some(2), 5, 1.0, 10), row(2, Some(1), 5, 1.0, 10)];
        assert_eq!(tree_usage(1, &table).2, 2);
    }

    #[test]
    fn sampling_reads_this_process() {
        let own = Pid::from_u32(std::process::id());
        let mut sys = System::new();
        let snap = sample(&mut sys, own, true);
        assert!(snap.manager_mem > 0);
        assert!(snap.table.iter().any(|r| r.pid == std::process::id()));
        assert!(sys.total_memory() > 0);

        let mut ring = VecDeque::new();
        for i in 0..(SAMPLES_KEPT + 20) {
            push_bounded(&mut ring, i);
        }
        assert_eq!(ring.len(), SAMPLES_KEPT);
    }
}
