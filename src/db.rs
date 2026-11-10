//! Embedded SQLite data layer. All access goes through `Db::call`, which runs
//! the closure on the blocking pool so the async runtime is never stalled.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

-- Application Targets Manifest (PRD section 4, plus supervisor columns)
CREATE TABLE IF NOT EXISTS portal_targets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    working_directory TEXT NOT NULL,
    script_name TEXT NOT NULL,
    git_branch TEXT NOT NULL DEFAULT 'main',
    current_git_status TEXT CHECK(current_git_status IN ('UP_TO_DATE', 'UPDATES_AVAILABLE', 'DIRTY')) DEFAULT 'UP_TO_DATE',
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    health_url TEXT,
    run_mode TEXT NOT NULL DEFAULT 'TASK' CHECK(run_mode IN ('TASK', 'SERVICE')),
    autostart INTEGER NOT NULL DEFAULT 0,
    timeout_secs INTEGER NOT NULL DEFAULT 300
);

-- Process Execution Performance Log
CREATE TABLE IF NOT EXISTS process_execution_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER,
    triggered_by TEXT NOT NULL DEFAULT 'SYSTEM_AUTO',
    status TEXT CHECK(status IN ('RUNNING', 'SUCCESS', 'FAILED')) NOT NULL,
    exit_code INTEGER,
    duration_ms INTEGER,
    captured_logs TEXT,
    executed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
);

-- Code Security Metrics
CREATE TABLE IF NOT EXISTS security_scan_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER,
    critical_issues_found INTEGER DEFAULT 0,
    vulnerable_packages_found INTEGER DEFAULT 0,
    raw_payload_json TEXT,
    scanned_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_history_target ON process_execution_history(target_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_scans_target ON security_scan_reports(target_id, id DESC);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub id: i64,
    pub name: String,
    pub working_directory: String,
    pub script_name: String,
    pub git_branch: String,
    pub current_git_status: String,
    pub created_at: String,
    pub health_url: Option<String>,
    pub run_mode: String,
    pub autostart: bool,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetInput {
    pub name: String,
    pub working_directory: String,
    pub script_name: String,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub health_url: Option<String>,
    #[serde(default)]
    pub run_mode: Option<String>,
    #[serde(default)]
    pub autostart: Option<bool>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub id: i64,
    pub target_id: i64,
    pub triggered_by: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub executed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_logs: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub id: i64,
    pub target_id: i64,
    pub critical_issues_found: i64,
    pub vulnerable_packages_found: i64,
    pub scanned_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_payload: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const TARGET_COLS: &str = "id, name, working_directory, script_name, git_branch, \
     current_git_status, created_at, health_url, run_mode, autostart, timeout_secs";

fn row_to_target(r: &Row) -> rusqlite::Result<Target> {
    Ok(Target {
        id: r.get(0)?,
        name: r.get(1)?,
        working_directory: r.get(2)?,
        script_name: r.get(3)?,
        git_branch: r.get(4)?,
        current_git_status: r
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| "UP_TO_DATE".into()),
        created_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        health_url: r.get(7)?,
        run_mode: r.get(8)?,
        autostart: r.get::<_, i64>(9)? != 0,
        timeout_secs: r.get::<_, i64>(10)?.max(0) as u64,
    })
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(SCHEMA)?;
        // Anything still RUNNING belongs to a previous manager process.
        conn.execute(
            "UPDATE process_execution_history SET status='FAILED', \
             captured_logs = COALESCE(captured_logs,'') || '\n[portalcontrol] manager restarted while this run was active' \
             WHERE status='RUNNING'",
            [],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure against the connection on the blocking thread pool.
    pub async fn call<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&guard)
        })
        .await
        .map_err(|e| format!("db task failed: {e}"))?
        .map_err(|e| format!("db error: {e}"))
    }

    pub async fn list_targets(&self) -> Result<Vec<Target>, String> {
        self.call(|c| {
            let mut st =
                c.prepare(&format!("SELECT {TARGET_COLS} FROM portal_targets ORDER BY id"))?;
            let rows = st.query_map([], row_to_target)?;
            rows.collect()
        })
        .await
    }

    pub async fn get_target(&self, id: i64) -> Result<Option<Target>, String> {
        self.call(move |c| {
            c.query_row(
                &format!("SELECT {TARGET_COLS} FROM portal_targets WHERE id=?1"),
                [id],
                row_to_target,
            )
            .optional()
        })
        .await
    }

    pub async fn insert_target(&self, t: TargetInput) -> Result<i64, String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO portal_targets (name, working_directory, script_name, git_branch, \
                 health_url, run_mode, autostart, timeout_secs) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    t.name,
                    t.working_directory,
                    t.script_name,
                    t.git_branch.unwrap_or_else(|| "main".into()),
                    t.health_url,
                    t.run_mode.unwrap_or_else(|| "TASK".into()),
                    t.autostart.unwrap_or(false) as i64,
                    t.timeout_secs.unwrap_or(300) as i64,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn update_target(&self, id: i64, t: TargetInput) -> Result<bool, String> {
        self.call(move |c| {
            let n = c.execute(
                "UPDATE portal_targets SET name=?1, working_directory=?2, script_name=?3, \
                 git_branch=?4, health_url=?5, run_mode=?6, autostart=?7, timeout_secs=?8 WHERE id=?9",
                params![
                    t.name,
                    t.working_directory,
                    t.script_name,
                    t.git_branch.unwrap_or_else(|| "main".into()),
                    t.health_url,
                    t.run_mode.unwrap_or_else(|| "TASK".into()),
                    t.autostart.unwrap_or(false) as i64,
                    t.timeout_secs.unwrap_or(300) as i64,
                    id
                ],
            )?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn delete_target(&self, id: i64) -> Result<bool, String> {
        self.call(move |c| Ok(c.execute("DELETE FROM portal_targets WHERE id=?1", [id])? > 0))
            .await
    }

    pub async fn set_git_status(&self, id: i64, status: &'static str) -> Result<(), String> {
        self.call(move |c| {
            c.execute(
                "UPDATE portal_targets SET current_git_status=?1 WHERE id=?2",
                params![status, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn start_run(&self, target_id: i64, triggered_by: String) -> Result<i64, String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO process_execution_history (target_id, triggered_by, status) \
                 VALUES (?1, ?2, 'RUNNING')",
                params![target_id, triggered_by],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn finish_run(
        &self,
        run_id: i64,
        status: &'static str,
        exit_code: Option<i64>,
        duration_ms: i64,
        logs: String,
    ) -> Result<(), String> {
        self.call(move |c| {
            c.execute(
                "UPDATE process_execution_history SET status=?1, exit_code=?2, duration_ms=?3, \
                 captured_logs=?4 WHERE id=?5",
                params![status, exit_code, duration_ms, logs, run_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_runs(&self, target_id: i64, limit: i64) -> Result<Vec<RunRecord>, String> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT id, target_id, triggered_by, status, exit_code, duration_ms, executed_at \
                 FROM process_execution_history WHERE target_id=?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![target_id, limit], |r| {
                Ok(RunRecord {
                    id: r.get(0)?,
                    target_id: r.get(1)?,
                    triggered_by: r.get(2)?,
                    status: r.get(3)?,
                    exit_code: r.get(4)?,
                    duration_ms: r.get(5)?,
                    executed_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    captured_logs: None,
                })
            })?;
            rows.collect()
        })
        .await
    }

    pub async fn get_run(&self, run_id: i64) -> Result<Option<RunRecord>, String> {
        self.call(move |c| {
            c.query_row(
                "SELECT id, target_id, triggered_by, status, exit_code, duration_ms, executed_at, \
                 captured_logs FROM process_execution_history WHERE id=?1",
                [run_id],
                |r| {
                    Ok(RunRecord {
                        id: r.get(0)?,
                        target_id: r.get(1)?,
                        triggered_by: r.get(2)?,
                        status: r.get(3)?,
                        exit_code: r.get(4)?,
                        duration_ms: r.get(5)?,
                        executed_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        captured_logs: Some(r.get::<_, Option<String>>(7)?.unwrap_or_default()),
                    })
                },
            )
            .optional()
        })
        .await
    }

    pub async fn insert_scan(
        &self,
        target_id: i64,
        critical: i64,
        vulnerable: i64,
        raw: String,
    ) -> Result<i64, String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO security_scan_reports (target_id, critical_issues_found, \
                 vulnerable_packages_found, raw_payload_json) VALUES (?1,?2,?3,?4)",
                params![target_id, critical, vulnerable, raw],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn latest_scan(
        &self,
        target_id: i64,
        with_payload: bool,
    ) -> Result<Option<ScanReport>, String> {
        self.call(move |c| {
            c.query_row(
                "SELECT id, target_id, critical_issues_found, vulnerable_packages_found, \
                 scanned_at, raw_payload_json FROM security_scan_reports \
                 WHERE target_id=?1 ORDER BY id DESC LIMIT 1",
                [target_id],
                |r| {
                    let raw: Option<String> = r.get(5)?;
                    Ok(ScanReport {
                        id: r.get(0)?,
                        target_id: r.get(1)?,
                        critical_issues_found: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        vulnerable_packages_found: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        scanned_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        raw_payload: if with_payload {
                            raw.and_then(|s| serde_json::from_str(&s).ok())
                        } else {
                            None
                        },
                    })
                },
            )
            .optional()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> TargetInput {
        TargetInput {
            name: name.into(),
            working_directory: "/tmp/x".into(),
            script_name: "deploy.sh".into(),
            git_branch: None,
            health_url: None,
            run_mode: None,
            autostart: None,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn target_crud_and_defaults() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("claims")).await.unwrap();
        let t = db.get_target(id).await.unwrap().unwrap();
        assert_eq!(t.git_branch, "main");
        assert_eq!(t.current_git_status, "UP_TO_DATE");
        assert_eq!(t.run_mode, "TASK");
        assert_eq!(t.timeout_secs, 300);
        assert!(!t.autostart);

        db.set_git_status(id, "DIRTY").await.unwrap();
        assert_eq!(db.list_targets().await.unwrap()[0].current_git_status, "DIRTY");

        assert!(db.delete_target(id).await.unwrap());
        assert!(db.get_target(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn history_cascades_on_target_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        let run = db.start_run(id, "MANUAL".into()).await.unwrap();
        db.finish_run(run, "SUCCESS", Some(0), 12, "hello".into())
            .await
            .unwrap();
        db.insert_scan(id, 1, 2, "{}".into()).await.unwrap();
        let r = db.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, "SUCCESS");
        assert_eq!(r.captured_logs.as_deref(), Some("hello"));
        assert_eq!(db.list_runs(id, 10).await.unwrap().len(), 1);

        db.delete_target(id).await.unwrap();
        assert!(db.get_run(run).await.unwrap().is_none());
        assert!(db.latest_scan(id, false).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_invalid_git_status() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        assert!(db.set_git_status(id, "BOGUS").await.is_err());
    }
}
