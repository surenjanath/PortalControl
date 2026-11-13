//! Resource metrics of the host, the manager itself and each supervised
//! process tree.
//!
//! * Every 5 seconds a sample goes into a small in-memory ring (15 minutes),
//!   which feeds the live charts.
//! * Once a minute those samples, together with the minute's health probes,
//!   are condensed into one database row per target, kept for 7 days. That
//!   feeds the 24-hour and 7-day views, availability and the outage list.

use crate::db::MetricPoint;
use crate::state::{AppState, HealthSample};
use crate::util::now_secs;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// Samples kept per series (15 minutes at the 5 s interval).
const SAMPLES_KEPT: usize = 180;
/// How long minute rows are kept in the database.
pub const RETENTION_SECS: i64 = 7 * 24 * 3600;

/// CPU is a percentage of the whole machine (all cores = 100); memory is
/// bytes; disk figures are bytes per second over the sample interval.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcSample {
    pub t: u64,
    pub cpu: f32,
    pub mem: u64,
    pub procs: u32,
    pub read_bps: u64,
    pub write_bps: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostSample {
    pub t: u64,
    pub cpu: f32,
    pub mem_used: u64,
    pub manager_cpu: f32,
    pub manager_mem: u64,
}

/// One process of a target's tree, for the process table.
#[derive(Debug, Clone, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiskInfo {
    pub mount: String,
    pub total: u64,
    pub free: u64,
}

/// Running totals of one target within the current minute.
#[derive(Debug, Clone, Default)]
pub struct MinuteAcc {
    cpu_sum: f64,
    mem_sum: f64,
    mem_max: u64,
    procs_max: u32,
    samples: u32,
}

impl MinuteAcc {
    fn add(&mut self, s: &ProcSample) {
        self.cpu_sum += f64::from(s.cpu);
        self.mem_sum += s.mem as f64;
        self.mem_max = self.mem_max.max(s.mem);
        self.procs_max = self.procs_max.max(s.procs);
        self.samples += 1;
    }
}

#[derive(Default)]
pub struct MetricsStore {
    pub host: VecDeque<HostSample>,
    pub targets: HashMap<i64, VecDeque<ProcSample>>,
    /// Latest process list of each running target, largest memory first.
    pub processes: HashMap<i64, Vec<ProcInfo>>,
    pub disks: Vec<DiskInfo>,
    minute: HashMap<i64, MinuteAcc>,
    pub mem_total: u64,
    pub cpus: usize,
    pub host_name: Option<String>,
    pub os_version: Option<String>,
}

impl MetricsStore {
    pub fn latest(&self, target_id: i64) -> Option<ProcSample> {
        self.targets.get(&target_id).and_then(|r| r.back().cloned())
    }

    /// The disk a path lives on: the mount point that is its longest prefix.
    pub fn disk_for(&self, path: &str) -> Option<DiskInfo> {
        let norm = |s: &str| s.replace('\\', "/").to_lowercase();
        let wanted = norm(path);
        self.disks
            .iter()
            .filter(|d| wanted.starts_with(&norm(&d.mount)))
            .max_by_key(|d| d.mount.len())
            .cloned()
    }
}

/// One row of the process table.
#[derive(Debug, Clone)]
pub struct ProcRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub start: u64,
    pub cpu: f32,
    pub mem: u64,
    pub name: String,
    /// Bytes read / written since the previous refresh.
    pub read: u64,
    pub written: u64,
}

/// `root` and all of its descendants. A child must not be older than its
/// parent, which guards against recycled process ids.
pub fn tree_rows(root: u32, table: &[ProcRow]) -> Vec<&ProcRow> {
    let by_pid: HashMap<u32, &ProcRow> = table.iter().map(|r| (r.pid, r)).collect();
    let mut children: HashMap<u32, Vec<&ProcRow>> = HashMap::new();
    for row in table {
        if let Some(parent) = row.parent {
            children.entry(parent).or_default().push(row);
        }
    }
    let Some(root_row) = by_pid.get(&root) else { return Vec::new() };
    let mut out = Vec::new();
    let mut stack = vec![*root_row];
    let mut seen = HashSet::new();
    while let Some(row) = stack.pop() {
        if !seen.insert(row.pid) {
            continue;
        }
        out.push(row);
        for child in children.get(&row.pid).into_iter().flatten() {
            if child.start >= row.start {
                stack.push(child);
            }
        }
    }
    out
}

/// CPU, memory and process count summed over the tree of `root`.
#[cfg(test)]
pub fn tree_usage(root: u32, table: &[ProcRow]) -> (f32, u64, u32) {
    let rows = tree_rows(root, table);
    (rows.iter().map(|r| r.cpu).sum(), rows.iter().map(|r| r.mem).sum(), rows.len() as u32)
}

/// Condense one target's minute into a database row. `health` holds the
/// probes that fell inside the minute. Returns `None` when there is nothing
/// to record (the target neither ran nor was probed).
pub fn minute_point(
    target_id: i64,
    minute_start: i64,
    acc: Option<&MinuteAcc>,
    health: &[HealthSample],
) -> Option<MetricPoint> {
    let acc = acc.filter(|a| a.samples > 0);
    if acc.is_none() && health.is_empty() {
        return None;
    }
    let latencies: Vec<u64> = health.iter().filter_map(|h| h.latency_ms).collect();
    let n = acc.map(|a| f64::from(a.samples)).unwrap_or(1.0);
    Some(MetricPoint {
        target_id,
        t: minute_start,
        cpu: acc.map(|a| a.cpu_sum / n),
        mem: acc.map(|a| (a.mem_sum / n) as i64),
        mem_max: acc.map(|a| a.mem_max as i64),
        procs: acc.map(|a| i64::from(a.procs_max)),
        latency: if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        probes: health.len() as i64,
        probes_up: health.iter().filter(|h| h.status != "DOWN").count() as i64,
    })
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
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage();
    if want_table {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    } else {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[own]), true, kind);
    }
    let table = if want_table {
        sys.processes()
            .iter()
            .map(|(pid, p)| {
                let io = p.disk_usage();
                ProcRow {
                    pid: pid.as_u32(),
                    parent: p.parent().map(|x| x.as_u32()),
                    start: p.start_time(),
                    cpu: p.cpu_usage(),
                    mem: p.memory(),
                    name: p.name().to_string_lossy().into_owned(),
                    read: io.read_bytes,
                    written: io.written_bytes,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let (manager_cpu, manager_mem) =
        sys.process(own).map(|p| (p.cpu_usage(), p.memory())).unwrap_or((0.0, 0));
    Snapshot {
        table,
        host_cpu: sys.global_cpu_usage(),
        mem_used: sys.used_memory(),
        manager_cpu,
        manager_mem,
    }
}

fn read_disks() -> Vec<DiskInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut out: Vec<DiskInfo> = disks
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| DiskInfo {
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            free: d.available_space(),
        })
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out.dedup_by(|a, b| a.mount == b.mount);
    out
}

/// Write the finished minute to the database and start a new one.
async fn flush_minute(state: &Arc<AppState>, minute_start: i64) {
    let accs: HashMap<i64, MinuteAcc> =
        state.metrics.lock().map(|mut m| std::mem::take(&mut m.minute)).unwrap_or_default();
    let (from, to) = (minute_start as u64, minute_start as u64 + 60);
    let probes: HashMap<i64, Vec<HealthSample>> = state
        .health_history
        .lock()
        .map(|h| {
            h.iter()
                .map(|(id, ring)| (*id, ring.iter().filter(|s| s.t >= from && s.t < to).cloned().collect()))
                .collect()
        })
        .unwrap_or_default();
    let ids: HashSet<i64> = accs.keys().chain(probes.keys()).copied().collect();
    let points: Vec<MetricPoint> = ids
        .into_iter()
        .filter_map(|id| {
            let health = probes.get(&id).map(Vec::as_slice).unwrap_or(&[]);
            minute_point(id, minute_start, accs.get(&id), health)
        })
        .collect();
    if !points.is_empty() {
        let _ = state.db.insert_metric_points(points).await;
    }
    // Once an hour is plenty for pruning.
    if minute_start % 3600 == 0 {
        let _ = state.db.prune_metric_points(minute_start - RETENTION_SECS).await;
    }
    if let Ok(disks) = tokio::task::spawn_blocking(read_disks).await {
        if let Ok(mut m) = state.metrics.lock() {
            m.disks = disks;
        }
    }
}

pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let own = Pid::from_u32(std::process::id());
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let cpus = sys.cpus().len().max(1);
        let disks = tokio::task::spawn_blocking(read_disks).await.unwrap_or_default();
        if let Ok(mut store) = state.metrics.lock() {
            store.mem_total = sys.total_memory();
            store.cpus = cpus;
            store.host_name = System::host_name();
            store.os_version = System::long_os_version();
            store.disks = disks;
        }
        let _ = state.db.prune_metric_points(now_secs() as i64 - RETENTION_SECS).await;

        let mut current_minute = now_secs() as i64 / 60;
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
            let secs = SAMPLE_INTERVAL.as_secs().max(1);
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
                store.processes.clear();
                for (target_id, pid) in &roots {
                    let rows = tree_rows(*pid, &snap.table);
                    let sample = ProcSample {
                        t,
                        cpu: rows.iter().map(|r| r.cpu).sum::<f32>() / per_core,
                        mem: rows.iter().map(|r| r.mem).sum(),
                        procs: rows.len() as u32,
                        read_bps: rows.iter().map(|r| r.read).sum::<u64>() / secs,
                        write_bps: rows.iter().map(|r| r.written).sum::<u64>() / secs,
                    };
                    store.minute.entry(*target_id).or_default().add(&sample);
                    push_bounded(store.targets.entry(*target_id).or_default(), sample);
                    let mut list: Vec<ProcInfo> = rows
                        .iter()
                        .map(|r| ProcInfo {
                            pid: r.pid,
                            name: r.name.clone(),
                            cpu: r.cpu / per_core,
                            mem: r.mem,
                        })
                        .collect();
                    list.sort_by_key(|p| std::cmp::Reverse(p.mem));
                    list.truncate(50);
                    store.processes.insert(*target_id, list);
                }
                // Forget series of targets that have been idle for a while.
                let horizon = t.saturating_sub(SAMPLE_INTERVAL.as_secs() * SAMPLES_KEPT as u64);
                store.targets.retain(|_, ring| ring.back().map(|s| s.t >= horizon).unwrap_or(false));
            }

            let minute = t as i64 / 60;
            if minute != current_minute {
                flush_minute(&state, current_minute * 60).await;
                current_minute = minute;
            }
        }
    });
}

/// Longest stretches in which every probe failed, newest first.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Outage {
    pub from: i64,
    pub to: i64,
    pub minutes: i64,
}

/// Find outages in minute rows (oldest first). Minutes without probes are
/// ignored; a gap of more than two minutes ends an outage.
pub fn outages(points: &[MetricPoint]) -> Vec<Outage> {
    let mut out: Vec<Outage> = Vec::new();
    let mut open: Option<(i64, i64)> = None;
    for p in points.iter().filter(|p| p.probes > 0) {
        let down = p.probes_up == 0;
        match (&mut open, down) {
            (Some((_, last)), true) if p.t - *last <= 120 => *last = p.t,
            (Some((from, last)), _) => {
                out.push(Outage { from: *from, to: *last + 60, minutes: (*last + 60 - *from) / 60 });
                open = down.then_some((p.t, p.t));
            }
            (None, true) => open = Some((p.t, p.t)),
            (None, false) => {}
        }
    }
    if let Some((from, last)) = open {
        out.push(Outage { from, to: last + 60, minutes: (last + 60 - from) / 60 });
    }
    out.reverse();
    out
}

/// Average rows into buckets of `bucket` seconds (oldest first).
pub fn bucketed(points: &[MetricPoint], bucket: i64) -> Vec<MetricPoint> {
    let mut out: Vec<MetricPoint> = Vec::new();
    let mut group: Vec<&MetricPoint> = Vec::new();
    let avg = |values: Vec<f64>| -> Option<f64> {
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let close = |group: &mut Vec<&MetricPoint>, out: &mut Vec<MetricPoint>| {
        let Some(first) = group.first() else { return };
        out.push(MetricPoint {
            target_id: first.target_id,
            t: first.t / bucket * bucket,
            cpu: avg(group.iter().filter_map(|p| p.cpu).collect()),
            mem: avg(group.iter().filter_map(|p| p.mem.map(|m| m as f64)).collect()).map(|m| m as i64),
            mem_max: group.iter().filter_map(|p| p.mem_max).max(),
            procs: group.iter().filter_map(|p| p.procs).max(),
            latency: avg(group.iter().filter_map(|p| p.latency).collect()),
            probes: group.iter().map(|p| p.probes).sum(),
            probes_up: group.iter().map(|p| p.probes_up).sum(),
        });
        group.clear();
    };
    for p in points {
        if group.first().map(|f| f.t / bucket != p.t / bucket).unwrap_or(false) {
            close(&mut group, &mut out);
        }
        group.push(p);
    }
    close(&mut group, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, parent: Option<u32>, start: u64, cpu: f32, mem: u64) -> ProcRow {
        ProcRow { pid, parent, start, cpu, mem, name: format!("p{pid}"), read: 0, written: 0 }
    }

    #[test]
    fn usage_is_summed_over_the_process_tree() {
        let table = vec![
            row(10, Some(1), 100, 1.0, 1_000),   // cmd.exe (root)
            row(11, Some(10), 101, 20.0, 5_000), // python
            row(12, Some(11), 102, 5.0, 2_000),  // worker
            row(13, Some(11), 102, 0.5, 500),
            row(20, Some(1), 90, 50.0, 9_000), // unrelated
            row(30, Some(10), 50, 9.0, 9_999), // older than its "parent": recycled pid
        ];
        let (cpu, mem, procs) = tree_usage(10, &table);
        assert!((cpu - 26.5).abs() < 0.001);
        assert_eq!(mem, 8_500);
        assert_eq!(procs, 4);
        assert_eq!(tree_usage(999, &table), (0.0, 0, 0));
        let mut names: Vec<&str> = tree_rows(10, &table).iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["p10", "p11", "p12", "p13"]);
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
        let me = snap.table.iter().find(|r| r.pid == std::process::id()).unwrap();
        assert!(!me.name.is_empty());
        assert!(sys.total_memory() > 0);

        let mut ring = VecDeque::new();
        for i in 0..(SAMPLES_KEPT + 20) {
            push_bounded(&mut ring, i);
        }
        assert_eq!(ring.len(), SAMPLES_KEPT);

        let disks = read_disks();
        assert!(!disks.is_empty() && disks.iter().all(|d| d.free <= d.total));
    }

    fn probe(t: u64, status: &'static str, latency_ms: Option<u64>) -> HealthSample {
        HealthSample { t, status, latency_ms }
    }

    #[test]
    fn a_minute_is_condensed_into_one_point() {
        let mut acc = MinuteAcc::default();
        for (cpu, mem) in [(2.0f32, 100u64), (4.0, 300)] {
            acc.add(&ProcSample { t: 0, cpu, mem, procs: 3, read_bps: 0, write_bps: 0 });
        }
        let health = [
            probe(60, "LIVE", Some(10)),
            probe(75, "SLOW", Some(30)),
            probe(90, "DOWN", None),
            probe(105, "LIVE", Some(20)),
        ];
        let p = minute_point(7, 60, Some(&acc), &health).unwrap();
        assert_eq!((p.target_id, p.t), (7, 60));
        assert_eq!(p.cpu, Some(3.0));
        assert_eq!((p.mem, p.mem_max, p.procs), (Some(200), Some(300), Some(3)));
        assert_eq!(p.latency, Some(20.0));
        assert_eq!((p.probes, p.probes_up), (4, 3));

        // Probed but not running: health only. Neither: nothing to store.
        let p = minute_point(7, 60, None, &health[..1]).unwrap();
        assert_eq!((p.cpu, p.mem, p.probes), (None, None, 1));
        assert!(minute_point(7, 60, None, &[]).is_none());
        assert!(minute_point(7, 60, Some(&MinuteAcc::default()), &[]).is_none());
    }

    fn pt(t: i64, probes: i64, up: i64, cpu: Option<f64>) -> MetricPoint {
        MetricPoint {
            target_id: 1,
            t,
            cpu,
            mem: cpu.map(|c| c as i64 * 10),
            mem_max: cpu.map(|c| c as i64 * 20),
            procs: Some(1),
            latency: None,
            probes,
            probes_up: up,
        }
    }

    #[test]
    fn outages_are_found_and_points_bucketed() {
        let points = vec![
            pt(0, 4, 4, Some(1.0)),
            pt(60, 4, 0, Some(3.0)), // down ...
            pt(120, 4, 0, None),
            pt(180, 0, 0, None),      // no probes this minute: ignored
            pt(240, 4, 0, None),      // ... still the same outage (gap of 2 minutes)
            pt(300, 4, 2, Some(5.0)), // partly up: outage over
            pt(900, 4, 0, None),      // a second, still open, outage
        ];
        assert_eq!(
            outages(&points),
            vec![Outage { from: 900, to: 960, minutes: 1 }, Outage { from: 60, to: 300, minutes: 4 }]
        );
        assert!(outages(&[pt(0, 4, 4, None)]).is_empty());

        let b = bucketed(&points, 300);
        assert_eq!(b.iter().map(|p| p.t).collect::<Vec<_>>(), vec![0, 300, 900]);
        assert_eq!(b[0].cpu, Some(2.0));
        assert_eq!((b[0].probes, b[0].probes_up), (16, 4));
        assert_eq!(b[0].mem_max, Some(60));
        assert_eq!(b[2].cpu, None);
    }

    #[test]
    fn disk_is_matched_by_longest_mount_prefix() {
        let store = MetricsStore {
            disks: vec![
                DiskInfo { mount: "C:\\".into(), total: 100, free: 40 },
                DiskInfo { mount: "D:\\".into(), total: 200, free: 10 },
                DiskInfo { mount: "/".into(), total: 300, free: 30 },
                DiskInfo { mount: "/home".into(), total: 400, free: 50 },
            ],
            ..Default::default()
        };
        assert_eq!(store.disk_for("c:\\Users\\ICT\\portal").unwrap().total, 100);
        assert_eq!(store.disk_for("D:/data").unwrap().total, 200);
        assert_eq!(store.disk_for("/home/ict/portal").unwrap().total, 400);
        assert_eq!(store.disk_for("/var/www").unwrap().total, 300);
        assert!(store.disk_for("E:\\x").is_none());
    }
}
