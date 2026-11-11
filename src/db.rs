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

-- Manager-wide settings (for example the dashboard password hash)
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_history_target ON process_execution_history(target_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_scans_target ON security_scan_reports(target_id, id DESC);
"#;

/// Columns added after the first release. Applied to both fresh and existing
/// databases, so an upgrade never loses registered targets or history.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("portal_targets", "description", "TEXT NOT NULL DEFAULT ''"),
    ("portal_targets", "project", "TEXT NOT NULL DEFAULT 'Default'"),
    ("portal_targets", "env_vars", "TEXT NOT NULL DEFAULT ''"),
    ("portal_targets", "schedule", "TEXT"),
    ("portal_targets", "auto_deploy", "INTEGER NOT NULL DEFAULT 0"),
    ("portal_targets", "auto_restart", "INTEGER NOT NULL DEFAULT 0"),
    ("portal_targets", "webhook_token", "TEXT"),
    ("process_execution_history", "commit_sha", "TEXT"),
    ("process_execution_history", "commit_message", "TEXT"),
    ("process_execution_history", "label", "TEXT"),
    ("portal_targets", "db_file", "TEXT"),
    ("portal_targets", "backup_on_deploy", "INTEGER NOT NULL DEFAULT 0"),
    ("portal_targets", "start_order", "INTEGER NOT NULL DEFAULT 0"),
];

/// Execution-history rows kept per target; older ones are pruned.
const RUNS_KEPT_PER_TARGET: i64 = 200;

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
    pub description: String,
    pub project: String,
    pub env_vars: String,
    pub schedule: Option<String>,
    pub auto_deploy: bool,
    pub auto_restart: bool,
    pub webhook_token: Option<String>,
    /// SQLite database inside the working directory that is backed up.
    pub db_file: Option<String>,
    pub backup_on_deploy: bool,
    /// Position in the project's start sequence (lower starts first).
    pub start_order: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
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
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub env_vars: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub auto_deploy: Option<bool>,
    #[serde(default)]
    pub auto_restart: Option<bool>,
    #[serde(default)]
    pub db_file: Option<String>,
    #[serde(default)]
    pub backup_on_deploy: Option<bool>,
    #[serde(default)]
    pub start_order: Option<i64>,
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
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    /// Set for one-off commands (e.g. "Apply migrations"); empty for script runs.
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
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
     current_git_status, created_at, health_url, run_mode, autostart, timeout_secs, \
     description, project, env_vars, schedule, auto_deploy, auto_restart, webhook_token, db_file, backup_on_deploy, start_order";

const RUN_COLS: &str = "h.id, h.target_id, h.triggered_by, h.status, h.exit_code, h.duration_ms, \
     h.executed_at, h.commit_sha, h.commit_message, h.label";

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
        description: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
        project: r
            .get::<_, Option<String>>(12)?
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "Default".into()),
        env_vars: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
        schedule: r.get::<_, Option<String>>(14)?.filter(|s| !s.is_empty()),
        auto_deploy: r.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
        auto_restart: r.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
        webhook_token: r.get::<_, Option<String>>(17)?.filter(|s| !s.is_empty()),
        db_file: r.get::<_, Option<String>>(18)?.filter(|s| !s.is_empty()),
        backup_on_deploy: r.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
        start_order: r.get::<_, Option<i64>>(20)?.unwrap_or(0),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum RunExtra {
    None,
    Logs,
    TargetName,
}

/// Maps the `RUN_COLS` projection; `extra` says what column 9 holds, if any.
fn row_to_run(r: &Row, extra: RunExtra) -> rusqlite::Result<RunRecord> {
    let tail: Option<String> = match extra {
        RunExtra::None => None,
        _ => r.get::<_, Option<String>>(10)?,
    };
    Ok(RunRecord {
        id: r.get(0)?,
        target_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
        triggered_by: r.get(2)?,
        status: r.get(3)?,
        exit_code: r.get(4)?,
        duration_ms: r.get(5)?,
        executed_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        commit_sha: r.get(7)?,
        commit_message: r.get(8)?,
        label: r.get(9)?,
        target_name: if extra == RunExtra::TargetName { tail.clone() } else { None },
        captured_logs: if extra == RunExtra::Logs {
            Some(tail.unwrap_or_default())
        } else {
            None
        },
    })
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> rusqlite::Result<()> {
    let exists = {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = st.query_map([], |r| r.get::<_, String>(1))?;
        let mut found = false;
        for name in names {
            if name? == column {
                found = true;
            }
        }
        found
    };
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(())
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
        for (table, column, decl) in MIGRATIONS {
            add_column_if_missing(&conn, table, column, decl)?;
        }
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_targets_webhook \
             ON portal_targets(webhook_token) WHERE webhook_token IS NOT NULL;",
        )?;
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

    pub async fn target_by_webhook(&self, token: String) -> Result<Option<Target>, String> {
        self.call(move |c| {
            c.query_row(
                &format!("SELECT {TARGET_COLS} FROM portal_targets WHERE webhook_token=?1"),
                [token],
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
                 health_url, run_mode, autostart, timeout_secs, description, project, env_vars, \
                 schedule, auto_deploy, auto_restart, db_file, backup_on_deploy, start_order) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![
                    t.name,
                    t.working_directory,
                    t.script_name,
                    t.git_branch.unwrap_or_else(|| "main".into()),
                    t.health_url,
                    t.run_mode.unwrap_or_else(|| "TASK".into()),
                    t.autostart.unwrap_or(false) as i64,
                    t.timeout_secs.unwrap_or(300) as i64,
                    t.description.unwrap_or_default(),
                    t.project.unwrap_or_else(|| "Default".into()),
                    t.env_vars.unwrap_or_default(),
                    t.schedule,
                    t.auto_deploy.unwrap_or(false) as i64,
                    t.auto_restart.unwrap_or(false) as i64,
                    t.db_file,
                    t.backup_on_deploy.unwrap_or(false) as i64,
                    t.start_order.unwrap_or(0),
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
                 git_branch=?4, health_url=?5, run_mode=?6, autostart=?7, timeout_secs=?8, \
                 description=?9, project=?10, env_vars=?11, schedule=?12, auto_deploy=?13, \
                 auto_restart=?14, db_file=?15, backup_on_deploy=?16, start_order=?17 WHERE id=?18",
                params![
                    t.name,
                    t.working_directory,
                    t.script_name,
                    t.git_branch.unwrap_or_else(|| "main".into()),
                    t.health_url,
                    t.run_mode.unwrap_or_else(|| "TASK".into()),
                    t.autostart.unwrap_or(false) as i64,
                    t.timeout_secs.unwrap_or(300) as i64,
                    t.description.unwrap_or_default(),
                    t.project.unwrap_or_else(|| "Default".into()),
                    t.env_vars.unwrap_or_default(),
                    t.schedule,
                    t.auto_deploy.unwrap_or(false) as i64,
                    t.auto_restart.unwrap_or(false) as i64,
                    t.db_file,
                    t.backup_on_deploy.unwrap_or(false) as i64,
                    t.start_order.unwrap_or(0),
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

    pub async fn get_setting(&self, key: &'static str) -> Result<Option<String>, String> {
        self.call(move |c| {
            c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
                .optional()
        })
        .await
    }

    /// Store a setting, or remove it when `value` is `None`.
    pub async fn set_setting(&self, key: &'static str, value: Option<String>) -> Result<(), String> {
        self.call(move |c| {
            match value {
                Some(v) => c.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![key, v],
                )?,
                None => c.execute("DELETE FROM settings WHERE key=?1", [key])?,
            };
            Ok(())
        })
        .await
    }

    pub async fn set_auto_deploy(&self, id: i64, on: bool) -> Result<(), String> {
        self.call(move |c| {
            c.execute(
                "UPDATE portal_targets SET auto_deploy=?1 WHERE id=?2",
                params![on as i64, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn set_webhook_token(&self, id: i64, token: Option<String>) -> Result<(), String> {
        self.call(move |c| {
            c.execute(
                "UPDATE portal_targets SET webhook_token=?1 WHERE id=?2",
                params![token, id],
            )?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn start_run(
        &self,
        target_id: i64,
        triggered_by: String,
        commit_sha: Option<String>,
        commit_message: Option<String>,
    ) -> Result<i64, String> {
        self.start_run_labeled(target_id, triggered_by, commit_sha, commit_message, None)
            .await
    }

    pub async fn start_run_labeled(
        &self,
        target_id: i64,
        triggered_by: String,
        commit_sha: Option<String>,
        commit_message: Option<String>,
        label: Option<String>,
    ) -> Result<i64, String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO process_execution_history \
                 (target_id, triggered_by, status, commit_sha, commit_message, label) \
                 VALUES (?1, ?2, 'RUNNING', ?3, ?4, ?5)",
                params![target_id, triggered_by, commit_sha, commit_message, label],
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

    /// Drop the oldest history rows of a target beyond the retention limit.
    pub async fn prune_runs(&self, target_id: i64) -> Result<usize, String> {
        self.call(move |c| {
            c.execute(
                "DELETE FROM process_execution_history WHERE target_id=?1 AND id NOT IN \
                 (SELECT id FROM process_execution_history WHERE target_id=?1 \
                  ORDER BY id DESC LIMIT ?2)",
                params![target_id, RUNS_KEPT_PER_TARGET],
            )
        })
        .await
    }

    /// Delete the finished runs of a target. A run in progress is kept.
    pub async fn clear_runs(&self, target_id: i64) -> Result<usize, String> {
        self.call(move |c| {
            c.execute(
                "DELETE FROM process_execution_history WHERE target_id=?1 AND status!='RUNNING'",
                [target_id],
            )
        })
        .await
    }

    pub async fn list_runs(&self, target_id: i64, limit: i64) -> Result<Vec<RunRecord>, String> {
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {RUN_COLS} FROM process_execution_history h \
                 WHERE h.target_id=?1 ORDER BY h.id DESC LIMIT ?2"
            ))?;
            let rows = st.query_map(params![target_id, limit], |r| row_to_run(r, RunExtra::None))?;
            rows.collect()
        })
        .await
    }

    /// Most recent runs across every target, with the target's name.
    pub async fn list_recent_runs(&self, limit: i64) -> Result<Vec<RunRecord>, String> {
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {RUN_COLS}, t.name FROM process_execution_history h \
                 JOIN portal_targets t ON t.id = h.target_id ORDER BY h.id DESC LIMIT ?1"
            ))?;
            let rows = st.query_map([limit], |r| row_to_run(r, RunExtra::TargetName))?;
            rows.collect()
        })
        .await
    }

    pub async fn get_run(&self, run_id: i64) -> Result<Option<RunRecord>, String> {
        self.call(move |c| {
            c.query_row(
                &format!(
                    "SELECT {RUN_COLS}, h.captured_logs FROM process_execution_history h \
                     WHERE h.id=?1"
                ),
                [run_id],
                |r| row_to_run(r, RunExtra::Logs),
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

    /// Scan summaries (no payload), newest first, for the trend view.
    pub async fn list_scans(&self, target_id: i64, limit: i64) -> Result<Vec<ScanReport>, String> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT id, target_id, critical_issues_found, vulnerable_packages_found, \
                 scanned_at FROM security_scan_reports WHERE target_id=?1 \
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![target_id, limit], |r| {
                Ok(ScanReport {
                    id: r.get(0)?,
                    target_id: r.get(1)?,
                    critical_issues_found: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    vulnerable_packages_found: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    scanned_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    raw_payload: None,
                })
            })?;
            rows.collect()
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
            ..Default::default()
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
        assert_eq!(t.project, "Default");
        assert!(!t.autostart && !t.auto_deploy && !t.auto_restart);
        assert!(t.schedule.is_none() && t.webhook_token.is_none() && t.env_vars.is_empty());

        db.set_git_status(id, "DIRTY").await.unwrap();
        assert_eq!(db.list_targets().await.unwrap()[0].current_git_status, "DIRTY");

        let mut edit = input("claims");
        edit.project = Some("Insurance".into());
        edit.env_vars = Some("DEBUG=1".into());
        edit.schedule = Some("0 2 * * *".into());
        edit.auto_deploy = Some(true);
        assert!(db.update_target(id, edit).await.unwrap());
        let t = db.get_target(id).await.unwrap().unwrap();
        assert_eq!(t.project, "Insurance");
        assert_eq!(t.env_vars, "DEBUG=1");
        assert_eq!(t.schedule.as_deref(), Some("0 2 * * *"));
        assert!(t.auto_deploy);

        assert!(db.delete_target(id).await.unwrap());
        assert!(db.get_target(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn webhook_token_lookup() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        assert!(db.target_by_webhook("abc".into()).await.unwrap().is_none());
        db.set_webhook_token(id, Some("abc".into())).await.unwrap();
        assert_eq!(db.target_by_webhook("abc".into()).await.unwrap().unwrap().id, id);
        db.set_webhook_token(id, None).await.unwrap();
        assert!(db.target_by_webhook("abc".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn history_cascades_on_target_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        let run = db
            .start_run(id, "MANUAL".into(), Some("abc123".into()), Some("fix".into()))
            .await
            .unwrap();
        db.finish_run(run, "SUCCESS", Some(0), 12, "hello".into())
            .await
            .unwrap();
        db.insert_scan(id, 1, 2, "{}".into()).await.unwrap();
        let r = db.get_run(run).await.unwrap().unwrap();
        assert_eq!(r.status, "SUCCESS");
        assert_eq!(r.captured_logs.as_deref(), Some("hello"));
        assert_eq!(r.commit_sha.as_deref(), Some("abc123"));
        assert_eq!(db.list_runs(id, 10).await.unwrap().len(), 1);
        let recent = db.list_recent_runs(10).await.unwrap();
        assert_eq!(recent[0].target_name.as_deref(), Some("a"));
        assert_eq!(db.list_scans(id, 10).await.unwrap().len(), 1);

        db.delete_target(id).await.unwrap();
        assert!(db.get_run(run).await.unwrap().is_none());
        assert!(db.latest_scan(id, false).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn history_is_pruned_and_cleared() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        for _ in 0..(RUNS_KEPT_PER_TARGET + 5) {
            let run = db.start_run(id, "MANUAL".into(), None, None).await.unwrap();
            db.finish_run(run, "SUCCESS", Some(0), 1, String::new()).await.unwrap();
        }
        assert_eq!(db.prune_runs(id).await.unwrap(), 5);
        let live = db.start_run(id, "MANUAL".into(), None, None).await.unwrap();
        assert_eq!(db.clear_runs(id).await.unwrap(), RUNS_KEPT_PER_TARGET as usize);
        let left = db.list_runs(id, 10).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, live);
    }

    #[tokio::test]
    async fn old_databases_are_migrated() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO portal_targets (name, working_directory, script_name) VALUES ('old','/x','d.sh')",
            [],
        )
        .unwrap();
        let db = Db::init(conn).unwrap();
        let t = &db.list_targets().await.unwrap()[0];
        assert_eq!(t.name, "old");
        assert_eq!(t.project, "Default");
        assert!(!t.auto_restart);
    }

    #[tokio::test]
    async fn rejects_invalid_git_status() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        assert!(db.set_git_status(id, "BOGUS").await.is_err());
    }
}
