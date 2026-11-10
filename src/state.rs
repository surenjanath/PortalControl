//! Shared in-memory state of the control plane.

use crate::db::Db;
use crate::git::GitInfo;
use crate::health::HealthInfo;
use crate::util::now_ms;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, Notify};

/// Hard caps that keep script output from growing without bound in RAM.
pub const MAX_LINE_BYTES: usize = 8 * 1024;
const TAIL_MAX_LINES: usize = 2_000;
const TAIL_MAX_BYTES: usize = 512 * 1024;

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
            out.push_str(&format!(
                "[portalcontrol] ... {} earlier line(s) dropped ...\n",
                self.dropped
            ));
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
    pub cancel: Arc<Notify>,
    pub tail: Arc<Mutex<LogTail>>,
}

pub struct AppState {
    pub db: Db,
    pub events: broadcast::Sender<String>,
    pub running: tokio::sync::Mutex<HashMap<i64, RunHandle>>,
    pub health: Mutex<HashMap<i64, HealthInfo>>,
    pub git: Mutex<HashMap<i64, GitInfo>>,
    pub scans_running: Mutex<HashSet<i64>>,
    pub started: Instant,
    pub ws_clients: AtomicUsize,
    pub bind: SocketAddr,
    pub shutdown: Notify,
}

impl AppState {
    pub fn new(db: Db, bind: SocketAddr) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            db,
            events,
            running: tokio::sync::Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            git: Mutex::new(HashMap::new()),
            scans_running: Mutex::new(HashSet::new()),
            started: Instant::now(),
            ws_clients: AtomicUsize::new(0),
            bind,
            shutdown: Notify::new(),
        })
    }

    /// Broadcast an event to every connected dashboard. Having no listeners is fine.
    pub fn emit(&self, event: serde_json::Value) {
        let _ = self.events.send(event.to_string());
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
}
