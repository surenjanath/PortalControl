//! Shared in-memory state of the control plane.

use crate::db::Db;
use crate::git::GitInfo;
use crate::health::HealthInfo;
use crate::metrics::MetricsStore;
use crate::util::now_ms;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::{broadcast, Notify};

/// Hard caps that keep script output from growing without bound in RAM.
pub const MAX_LINE_BYTES: usize = 8 * 1024;
const TAIL_MAX_LINES: usize = 2_000;
const TAIL_MAX_BYTES: usize = 512 * 1024;

/// Health probes remembered per target (one hour at the 15 s probe interval).
pub const HEALTH_HISTORY: usize = 240;
/// Entries kept in the activity feed.
const ACTIVITY_MAX: usize = 300;

/// Bounded ring of the most recent output lines of one run.
#[derive(Default)]
pub struct LogTail {
    lines: VecDeque<String>,
    bytes: usize,
    pub dropped: u64,
}

impl LogTail {
    pub fn push(&mut self, line: String) {
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > TAIL_MAX_LINES || self.bytes > TAIL_MAX_BYTES {
            match self.lines.pop_front() {
                Some(old) => {
                    self.bytes -= old.len() + 1;
                    self.dropped += 1;
                }
                None => break,
            }
        }
    }

    pub fn joined(&self) -> String {
        let mut out = String::with_capacity(self.bytes + 64);
        if self.dropped > 0 {
            out.push_str(&format!("[portalcontrol] ... {} earlier line(s) dropped ...\n", self.dropped));
        }
        for l in &self.lines {
            out.push_str(l);
            out.push('\n');
        }
        out
    }
}

pub struct RunHandle {
    pub run_id: i64,
    pub pid: Option<u32>,
    pub started: Instant,
    pub started_ms: u64,
    pub triggered_by: String,
    pub cancel: Arc<Notify>,
    pub tail: Arc<Mutex<LogTail>>,
}

/// One health probe result, kept for the latency chart and uptime figure.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSample {
    pub t: u64,
    pub status: &'static str,
    pub latency_ms: Option<u64>,
}

pub struct AppState {
    pub db: Db,
    pub events: broadcast::Sender<String>,
    pub running: tokio::sync::Mutex<HashMap<i64, RunHandle>>,
    pub health: Mutex<HashMap<i64, HealthInfo>>,
    pub health_history: Mutex<HashMap<i64, VecDeque<HealthSample>>>,
    pub git: Mutex<HashMap<i64, GitInfo>>,
    pub scans_running: Mutex<HashSet<i64>>,
    pub metrics: Mutex<MetricsStore>,
    pub activity: Mutex<VecDeque<Value>>,
    activity_seq: AtomicU64,
    /// Services waiting to be restarted after an unexpected exit: (target, due).
    pub restart_queue: Mutex<Vec<(i64, Instant)>>,
    /// Recent automatic restarts per target, for crash-loop detection.
    pub restart_log: Mutex<HashMap<i64, Vec<Instant>>>,
    /// Remote commit an auto-deploy last failed on, so it is not retried forever.
    pub auto_deploy_failed: Mutex<HashMap<i64, String>>,
    pub auth: crate::auth::Auth,
    /// Projects whose ordered start sequence is in progress.
    pub starting_projects: Mutex<HashSet<String>>,
    pub started: Instant,
    pub ws_clients: AtomicUsize,
    pub bind: SocketAddr,
    pub shutdown: Notify,
    pub data_dir: OnceLock<PathBuf>,
    /// The `serve` options this manager was started with (for login autostart).
    pub serve_args: OnceLock<Vec<String>>,
    /// What this manager is listening on (see `netcfg.rs`).
    pub net: OnceLock<crate::netcfg::Active>,
    /// `--bind` and `--port` as given on the command line.
    pub cli_net: OnceLock<(Option<std::net::IpAddr>, Option<u16>)>,
    /// Limits on other computers that can change while running.
    pub net_limits: Mutex<crate::netcfg::Limits>,
    /// Computers that used the network address since the start.
    pub clients: Mutex<HashMap<std::net::IpAddr, crate::netcfg::ClientSeen>>,
    /// Set when the manager should start again after it has stopped.
    pub restart: std::sync::atomic::AtomicBool,
}

impl AppState {
    pub fn new(db: Db, bind: SocketAddr) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            db,
            events,
            running: tokio::sync::Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            health_history: Mutex::new(HashMap::new()),
            git: Mutex::new(HashMap::new()),
            scans_running: Mutex::new(HashSet::new()),
            metrics: Mutex::new(MetricsStore::default()),
            activity: Mutex::new(VecDeque::new()),
            activity_seq: AtomicU64::new(1),
            restart_queue: Mutex::new(Vec::new()),
            restart_log: Mutex::new(HashMap::new()),
            auto_deploy_failed: Mutex::new(HashMap::new()),
            auth: crate::auth::Auth::default(),
            starting_projects: Mutex::new(HashSet::new()),
            started: Instant::now(),
            ws_clients: AtomicUsize::new(0),
            bind,
            shutdown: Notify::new(),
            data_dir: OnceLock::new(),
            serve_args: OnceLock::new(),
            net: OnceLock::new(),
            cli_net: OnceLock::new(),
            net_limits: Mutex::new(crate::netcfg::Limits::default()),
            clients: Mutex::new(HashMap::new()),
            restart: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Broadcast an event to every connected dashboard. Having no listeners is fine.
    pub fn emit(&self, event: Value) {
        let _ = self.events.send(event.to_string());
    }

    /// Record a notable event in the activity feed and push it to dashboards.
    /// `level` is one of `info`, `ok`, `warn`, `error`.
    pub fn activity(&self, level: &str, kind: &str, target_id: Option<i64>, message: impl Into<String>) {
        let event = json!({
            "type": "activity",
            "id": self.activity_seq.fetch_add(1, Ordering::Relaxed),
            "ts": now_ms(),
            "level": level,
            "kind": kind,
            "target_id": target_id,
            "message": message.into(),
        });
        if let Ok(mut ring) = self.activity.lock() {
            ring.push_back(event.clone());
            while ring.len() > ACTIVITY_MAX {
                ring.pop_front();
            }
        }
        self.emit(event);
    }

    /// Record one output line: append to the bounded tail and stream it out.
    pub fn log_line(
        &self,
        target_id: i64,
        run_id: i64,
        stream: &str,
        line: &str,
        tail: Option<&Mutex<LogTail>>,
    ) {
        if let Some(tail) = tail {
            if let Ok(mut t) = tail.lock() {
                t.push(line.to_string());
            }
        }
        self.emit(json!({
            "type": "log",
            "target_id": target_id,
            "run_id": run_id,
            "stream": stream,
            "line": line,
            "ts": now_ms(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_is_bounded() {
        let mut t = LogTail::default();
        for i in 0..(TAIL_MAX_LINES + 500) {
            t.push(format!("line {i}"));
        }
        assert_eq!(t.lines.len(), TAIL_MAX_LINES);
        assert_eq!(t.dropped, 500);
        let big = "x".repeat(MAX_LINE_BYTES);
        for _ in 0..200 {
            t.push(big.clone());
        }
        assert!(t.bytes <= TAIL_MAX_BYTES);
        assert!(t.joined().starts_with("[portalcontrol] ..."));
    }

    #[test]
    fn activity_feed_is_bounded_and_ordered() {
        let state = AppState::new(crate::db::Db::open_in_memory().unwrap(), "127.0.0.1:0".parse().unwrap());
        for i in 0..(ACTIVITY_MAX + 10) {
            state.activity("info", "test", Some(1), format!("event {i}"));
        }
        let ring = state.activity.lock().unwrap();
        assert_eq!(ring.len(), ACTIVITY_MAX);
        assert_eq!(ring.back().unwrap()["message"], format!("event {}", ACTIVITY_MAX + 9));
        assert!(ring.front().unwrap()["id"].as_u64() < ring.back().unwrap()["id"].as_u64());
    }
}
