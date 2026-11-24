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

-- One row per target per minute: resource use and health, kept for 7 days
CREATE TABLE IF NOT EXISTS metric_points (
    target_id INTEGER NOT NULL,
    t INTEGER NOT NULL,              -- unix seconds, start of the minute
    cpu REAL,                        -- average % of the whole machine
    mem INTEGER,                     -- average bytes
    mem_max INTEGER,
    procs INTEGER,
    latency REAL,                    -- average health response time, ms
    probes INTEGER NOT NULL DEFAULT 0,
    probes_up INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_id, t),
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_history_target ON process_execution_history(target_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_scans_target ON security_scan_reports(target_id, id DESC);

-- Problems the monitor found (monitor.rs), with the AI's analysis when it ran.
CREATE TABLE IF NOT EXISTS incidents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    trigger TEXT NOT NULL,           -- what the rules saw: failure, crash_loop, health_down, error_burst, new_error
    severity TEXT NOT NULL,          -- info, warning, critical
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    cause TEXT NOT NULL DEFAULT '',
    action TEXT NOT NULL DEFAULT 'none',   -- one of the actions that were offered, or none
    action_label TEXT NOT NULL DEFAULT '',
    action_reason TEXT NOT NULL DEFAULT '',
    analysed_by TEXT NOT NULL DEFAULT '',  -- model name, or '' for rules only
    excerpt TEXT NOT NULL DEFAULT '',      -- the log lines that were looked at
    status TEXT NOT NULL DEFAULT 'open',   -- open, resolved, dismissed
    note TEXT NOT NULL DEFAULT '',
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_incidents_status ON incidents(status, id DESC);

-- The trace of one incident, from log lines to fix: what the rules saw, what
-- was asked of the AI model and what it answered, the email and the fix.
CREATE TABLE IF NOT EXISTS ai_traces (
    trace_id TEXT PRIMARY KEY,       -- TR-<local date>-<time>-<4 hex>
    incident_id INTEGER,
    target_id INTEGER,
    run_id INTEGER,                  -- the run whose output triggered it
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    trigger TEXT NOT NULL,
    what TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',  -- '' when no model was asked
    prompt TEXT NOT NULL DEFAULT '',
    answer TEXT NOT NULL DEFAULT '', -- the model's raw answer
    status TEXT NOT NULL DEFAULT 'rules',  -- rules (no model), ok, failed
    error TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER,
    prompt_tokens INTEGER,
    answer_tokens INTEGER,
    email TEXT NOT NULL DEFAULT '',  -- what happened to the alert email
    applied TEXT NOT NULL DEFAULT '' -- the fix someone applied
);
CREATE INDEX IF NOT EXISTS idx_traces_incident ON ai_traces(incident_id);
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
    ("portal_targets", "python_env", "TEXT"),
    ("portal_targets", "webhook_secret", "TEXT"),
    ("incidents", "trace_id", "TEXT NOT NULL DEFAULT ''"),
    ("incidents", "run_id", "INTEGER"),
    ("portal_targets", "git_dir", "TEXT"),
    ("incidents", "details", "TEXT NOT NULL DEFAULT ''"),
];

/// Execution-history rows kept per target; older ones are pruned.
const RUNS_KEPT_PER_TARGET: i64 = 200;

#[derive(Debug, Clone, Default, Serialize)]
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
    /// Python environment activated for runs: `None` for none, `auto`, or a
    /// folder. See `pyenv.rs`.
    pub python_env: Option<String>,
    /// Optional secret a webhook caller must prove (an HMAC signature or a
    /// header), on top of the token in the URL.
    pub webhook_secret: Option<String>,
    /// Where the Git repository is: `None` for the working directory, `auto`
    /// to search upward from it, or a folder. See `git::repo_dir`.
    pub git_dir: Option<String>,
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
    #[serde(default)]
    pub python_env: Option<String>,
    #[serde(default)]
    pub git_dir: Option<String>,
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

/// One minute of a target's metrics (or, after bucketing, one bucket).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MetricPoint {
    pub target_id: i64,
    pub t: i64,
    pub cpu: Option<f64>,
    pub mem: Option<i64>,
    pub mem_max: Option<i64>,
    pub procs: Option<i64>,
    pub latency: Option<f64>,
    pub probes: i64,
    pub probes_up: i64,
}

/// The trace of one incident (see `monitor.rs`).
#[derive(Debug, Clone, Serialize, Default)]
pub struct Trace {
    pub trace_id: String,
    pub incident_id: Option<i64>,
    pub target_id: Option<i64>,
    pub run_id: Option<i64>,
    pub created_at: String,
    pub trigger: String,
    pub what: String,
    pub model: String,
    pub prompt: String,
    pub answer: String,
    pub status: String,
    pub error: String,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub answer_tokens: Option<i64>,
    pub email: String,
    pub applied: String,
}

/// One incident (see `monitor.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Incident {
    pub id: i64,
    pub target_id: Option<i64>,
    pub created_at: String,
    pub trigger: String,
    pub severity: String,
    pub title: String,
    pub summary: String,
    pub cause: String,
    pub action: String,
    pub action_label: String,
    pub action_reason: String,
    pub analysed_by: String,
    pub excerpt: String,
    pub status: String,
    pub note: String,
    /// Links the incident to its `ai_traces` record.
    pub trace_id: String,
    /// The run whose output triggered it.
    pub run_id: Option<i64>,
    /// Everything known about the moment it was opened (added in 0.14.0):
    /// the run, health and Git state, the key error line, how often it
    /// happened before, the rules' diagnosis and the AI's extra fields. An
    /// object; `null` for incidents from before 0.14.0. See `monitor.rs`.
    #[serde(default)]
    pub details: serde_json::Value,
}

const INCIDENT_COLS: &str = "id, target_id, created_at, trigger, severity, title, summary, cause, action, \
     action_label, action_reason, analysed_by, excerpt, status, note, trace_id, run_id, details";
/// Incidents kept; older ones are pruned.
const INCIDENTS_KEPT: i64 = 500;

fn row_to_incident(r: &Row) -> rusqlite::Result<Incident> {
    Ok(Incident {
        id: r.get(0)?,
        target_id: r.get(1)?,
        created_at: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        trigger: r.get(3)?,
        severity: r.get(4)?,
        title: r.get(5)?,
        summary: r.get(6)?,
        cause: r.get(7)?,
        action: r.get(8)?,
        action_label: r.get(9)?,
        action_reason: r.get(10)?,
        analysed_by: r.get(11)?,
        excerpt: r.get(12)?,
        status: r.get(13)?,
        note: r.get(14)?,
        trace_id: r.get(15)?,
        run_id: r.get(16)?,
        details: r
            .get::<_, Option<String>>(17)?
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(serde_json::Value::Null),
    })
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const TARGET_COLS: &str = "id, name, working_directory, script_name, git_branch, \
     current_git_status, created_at, health_url, run_mode, autostart, timeout_secs, \
     description, project, env_vars, schedule, auto_deploy, auto_restart, webhook_token, db_file, backup_on_deploy, start_order, python_env, webhook_secret, git_dir";

const RUN_COLS: &str = "h.id, h.target_id, h.triggered_by, h.status, h.exit_code, h.duration_ms, \
     h.executed_at, h.commit_sha, h.commit_message, h.label";

/// Column names double as the associated data of their sealed values.
const ENV_AAD: &str = "env_vars";
const WEBHOOK_SECRET_AAD: &str = "webhook_secret";

static UNREADABLE_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn log_unreadable() {
    if !UNREADABLE_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::util::log(&format!(
            "some stored secrets cannot be decrypted: {} in the data folder is missing or is not the key they were \
             saved with. Restore that file, or enter the environment variables and webhook secrets again.",
            crate::secrets::KEY_FILE
        ));
    }
}

/// Environment variables as stored. If they cannot be decrypted they read as
/// empty (and `update_target` will not overwrite them with that emptiness).
fn unseal_env(stored: &str) -> String {
    crate::secrets::open(stored, ENV_AAD).unwrap_or_else(|| {
        log_unreadable();
        String::new()
    })
}

/// A webhook signing secret that cannot be decrypted fails closed: it becomes
/// a random value nobody knows, so every delivery is refused rather than
/// accepted without a secret.
fn unseal_webhook_secret(stored: &str) -> String {
    crate::secrets::open(stored, WEBHOOK_SECRET_AAD).unwrap_or_else(|| {
        log_unreadable();
        crate::auth::random_hex(32)
    })
}

/// Seal every secret still stored in plaintext (from before encryption).
fn seal_plaintext_secrets(conn: &Connection) -> rusqlite::Result<()> {
    for (column, aad) in [("env_vars", ENV_AAD), ("webhook_secret", WEBHOOK_SECRET_AAD)] {
        let rows: Vec<(i64, String)> = conn
            .prepare(&format!(
                "SELECT id, {column} FROM portal_targets WHERE {column} IS NOT NULL AND {column} != '' \
                 AND {column} NOT LIKE 'enc1:%'"
            ))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, plain) in rows {
            conn.execute(
                &format!("UPDATE portal_targets SET {column}=?1 WHERE id=?2"),
                params![crate::secrets::seal(&plain, aad), id],
            )?;
        }
    }
    Ok(())
}

fn row_to_target(r: &Row) -> rusqlite::Result<Target> {
    Ok(Target {
        id: r.get(0)?,
        name: r.get(1)?,
        working_directory: r.get(2)?,
        script_name: r.get(3)?,
        git_branch: r.get(4)?,
        current_git_status: r.get::<_, Option<String>>(5)?.unwrap_or_else(|| "UP_TO_DATE".into()),
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
        env_vars: unseal_env(&r.get::<_, Option<String>>(13)?.unwrap_or_default()),
        schedule: r.get::<_, Option<String>>(14)?.filter(|s| !s.is_empty()),
        auto_deploy: r.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
        auto_restart: r.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
        webhook_token: r.get::<_, Option<String>>(17)?.filter(|s| !s.is_empty()),
        db_file: r.get::<_, Option<String>>(18)?.filter(|s| !s.is_empty()),
        backup_on_deploy: r.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
        start_order: r.get::<_, Option<i64>>(20)?.unwrap_or(0),
        python_env: r.get::<_, Option<String>>(21)?.filter(|s| !s.is_empty()),
        webhook_secret: r
            .get::<_, Option<String>>(22)?
            .filter(|s| !s.is_empty())
            .map(|s| unseal_webhook_secret(&s)),
        git_dir: r.get::<_, Option<String>>(23)?.filter(|s| !s.is_empty()),
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
        captured_logs: if extra == RunExtra::Logs { Some(tail.unwrap_or_default()) } else { None },
    })
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
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
    /// Open the database at `path`. The encryption key for stored secrets
    /// lives next to it and is created on first use.
    pub fn open(path: &Path) -> Result<Self, String> {
        crate::secrets::load_or_create(path.parent().unwrap_or(Path::new(".")))?;
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::init(conn).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        crate::secrets::ensure_test_key();
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        // Overwritten and deleted content is zeroed on disk, so plaintext
        // replaced by its sealed form does not linger in free pages.
        let _ = conn.pragma_update(None, "secure_delete", "ON");
        conn.execute_batch(SCHEMA)?;
        for (table, column, decl) in MIGRATIONS {
            add_column_if_missing(&conn, table, column, decl)?;
        }
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_targets_webhook \
             ON portal_targets(webhook_token) WHERE webhook_token IS NOT NULL;",
        )?;
        seal_plaintext_secrets(&conn)?;
        // Anything still RUNNING belongs to a previous manager process.
        conn.execute(
            "UPDATE process_execution_history SET status='FAILED', \
             captured_logs = COALESCE(captured_logs,'') || '\n[portalcontrol] manager restarted while this run was active' \
             WHERE status='RUNNING'",
            [],
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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
            let mut st = c.prepare(&format!("SELECT {TARGET_COLS} FROM portal_targets ORDER BY id"))?;
            let rows = st.query_map([], row_to_target)?;
            rows.collect()
        })
        .await
    }

    pub async fn get_target(&self, id: i64) -> Result<Option<Target>, String> {
        self.call(move |c| {
            c.query_row(&format!("SELECT {TARGET_COLS} FROM portal_targets WHERE id=?1"), [id], row_to_target)
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
                 schedule, auto_deploy, auto_restart, db_file, backup_on_deploy, start_order, python_env, git_dir) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
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
                    crate::secrets::seal(&t.env_vars.unwrap_or_default(), ENV_AAD),
                    t.schedule,
                    t.auto_deploy.unwrap_or(false) as i64,
                    t.auto_restart.unwrap_or(false) as i64,
                    t.db_file,
                    t.backup_on_deploy.unwrap_or(false) as i64,
                    t.start_order.unwrap_or(0),
                    t.python_env,
                    t.git_dir,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn update_target(&self, id: i64, t: TargetInput) -> Result<bool, String> {
        self.call(move |c| {
            let new_env = t.env_vars.clone().unwrap_or_default();
            let stored_env: Option<String> = c
                .query_row("SELECT env_vars FROM portal_targets WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            // Variables that cannot be decrypted show as empty; saving other
            // settings must not replace them with that emptiness.
            let env_to_store = match stored_env {
                Some(old) if new_env.is_empty() && crate::secrets::is_sealed(&old) && crate::secrets::open(&old, ENV_AAD).is_none() => old,
                _ => crate::secrets::seal(&new_env, ENV_AAD),
            };
            let n = c.execute(
                "UPDATE portal_targets SET name=?1, working_directory=?2, script_name=?3, \
                 git_branch=?4, health_url=?5, run_mode=?6, autostart=?7, timeout_secs=?8, \
                 description=?9, project=?10, env_vars=?11, schedule=?12, auto_deploy=?13, \
                 auto_restart=?14, db_file=?15, backup_on_deploy=?16, start_order=?17, python_env=?18, git_dir=?19 WHERE id=?20",
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
                    env_to_store,
                    t.schedule,
                    t.auto_deploy.unwrap_or(false) as i64,
                    t.auto_restart.unwrap_or(false) as i64,
                    t.db_file,
                    t.backup_on_deploy.unwrap_or(false) as i64,
                    t.start_order.unwrap_or(0),
                    t.python_env,
                    t.git_dir,
                    id
                ],
            )?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn delete_target(&self, id: i64) -> Result<bool, String> {
        self.call(move |c| Ok(c.execute("DELETE FROM portal_targets WHERE id=?1", [id])? > 0)).await
    }

    pub async fn set_git_status(&self, id: i64, status: &'static str) -> Result<(), String> {
        self.call(move |c| {
            c.execute("UPDATE portal_targets SET current_git_status=?1 WHERE id=?2", params![status, id])?;
            Ok(())
        })
        .await
    }

    /// Store minute points. A point for a target that no longer exists is skipped.
    pub async fn insert_metric_points(&self, points: Vec<MetricPoint>) -> Result<usize, String> {
        self.call(move |c| {
            let mut st = c.prepare(
                "INSERT OR REPLACE INTO metric_points \
                 (target_id, t, cpu, mem, mem_max, procs, latency, probes, probes_up) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            let mut stored = 0;
            for p in &points {
                let row = params![
                    p.target_id,
                    p.t,
                    p.cpu,
                    p.mem,
                    p.mem_max,
                    p.procs,
                    p.latency,
                    p.probes,
                    p.probes_up
                ];
                if st.execute(row).is_ok() {
                    stored += 1;
                }
            }
            Ok(stored)
        })
        .await
    }

    /// Minute points of a target from `since` (unix seconds) on, oldest first.
    pub async fn metric_points_since(&self, target_id: i64, since: i64) -> Result<Vec<MetricPoint>, String> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT target_id, t, cpu, mem, mem_max, procs, latency, probes, probes_up \
                 FROM metric_points WHERE target_id=?1 AND t>=?2 ORDER BY t",
            )?;
            let rows = st.query_map(params![target_id, since], |r| {
                Ok(MetricPoint {
                    target_id: r.get(0)?,
                    t: r.get(1)?,
                    cpu: r.get(2)?,
                    mem: r.get(3)?,
                    mem_max: r.get(4)?,
                    procs: r.get(5)?,
                    latency: r.get(6)?,
                    probes: r.get(7)?,
                    probes_up: r.get(8)?,
                })
            })?;
            rows.collect()
        })
        .await
    }

    pub async fn prune_metric_points(&self, before: i64) -> Result<usize, String> {
        self.call(move |c| c.execute("DELETE FROM metric_points WHERE t < ?1", [before])).await
    }

    pub async fn get_setting(&self, key: &'static str) -> Result<Option<String>, String> {
        self.call(move |c| {
            c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).optional()
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
            c.execute("UPDATE portal_targets SET auto_deploy=?1 WHERE id=?2", params![on as i64, id])?;
            Ok(())
        })
        .await
    }

    pub async fn insert_incident(&self, i: Incident) -> Result<i64, String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO incidents (target_id, trigger, severity, title, summary, cause, action, action_label, \
                 action_reason, analysed_by, excerpt, trace_id, run_id, details) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    i.target_id,
                    i.trigger,
                    i.severity,
                    i.title,
                    i.summary,
                    i.cause,
                    i.action,
                    i.action_label,
                    i.action_reason,
                    i.analysed_by,
                    i.excerpt,
                    i.trace_id,
                    i.run_id,
                    if i.details.is_null() { String::new() } else { i.details.to_string() }
                ],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM incidents WHERE id NOT IN (SELECT id FROM incidents ORDER BY id DESC LIMIT ?1)",
                [INCIDENTS_KEPT],
            )?;
            Ok(id)
        })
        .await
    }

    /// Newest first; `open_only` leaves out resolved and dismissed ones.
    pub async fn list_incidents(&self, open_only: bool, limit: i64) -> Result<Vec<Incident>, String> {
        self.call(move |c| {
            let sql = format!(
                "SELECT {INCIDENT_COLS} FROM incidents {} ORDER BY id DESC LIMIT ?1",
                if open_only { "WHERE status='open'" } else { "" }
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map([limit], row_to_incident)?;
            rows.collect()
        })
        .await
    }

    pub async fn get_incident(&self, id: i64) -> Result<Option<Incident>, String> {
        self.call(move |c| {
            c.query_row(&format!("SELECT {INCIDENT_COLS} FROM incidents WHERE id=?1"), [id], row_to_incident)
                .optional()
        })
        .await
    }

    pub async fn set_incident_status(
        &self,
        id: i64,
        status: &'static str,
        note: String,
    ) -> Result<bool, String> {
        self.call(move |c| {
            Ok(c.execute("UPDATE incidents SET status=?1, note=?2 WHERE id=?3", params![status, note, id])?
                > 0)
        })
        .await
    }

    pub async fn insert_trace(&self, t: Trace) -> Result<(), String> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO ai_traces (trace_id, incident_id, target_id, run_id, trigger, what, model, prompt, answer, \
                 status, error, duration_ms, prompt_tokens, answer_tokens) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    t.trace_id, t.incident_id, t.target_id, t.run_id, t.trigger, t.what, t.model, t.prompt, t.answer,
                    t.status, t.error, t.duration_ms, t.prompt_tokens, t.answer_tokens
                ],
            )?;
            c.execute(
                "DELETE FROM ai_traces WHERE trace_id NOT IN \
                 (SELECT trace_id FROM ai_traces ORDER BY created_at DESC, rowid DESC LIMIT ?1)",
                [INCIDENTS_KEPT],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_trace(&self, trace_id: String) -> Result<Option<Trace>, String> {
        self.call(move |c| {
            c.query_row(
                "SELECT trace_id, incident_id, target_id, run_id, created_at, trigger, what, model, prompt, answer, \
                 status, error, duration_ms, prompt_tokens, answer_tokens, email, applied FROM ai_traces WHERE trace_id=?1",
                [trace_id],
                |r| {
                    Ok(Trace {
                        trace_id: r.get(0)?,
                        incident_id: r.get(1)?,
                        target_id: r.get(2)?,
                        run_id: r.get(3)?,
                        created_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        trigger: r.get(5)?,
                        what: r.get(6)?,
                        model: r.get(7)?,
                        prompt: r.get(8)?,
                        answer: r.get(9)?,
                        status: r.get(10)?,
                        error: r.get(11)?,
                        duration_ms: r.get(12)?,
                        prompt_tokens: r.get(13)?,
                        answer_tokens: r.get(14)?,
                        email: r.get(15)?,
                        applied: r.get(16)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Record a later step of a trace: `email` or `applied`.
    pub async fn trace_note(
        &self,
        trace_id: String,
        column: &'static str,
        text: String,
    ) -> Result<(), String> {
        debug_assert!(column == "email" || column == "applied");
        self.call(move |c| {
            c.execute(
                &format!("UPDATE ai_traces SET {column}=?1 WHERE trace_id=?2"),
                params![text, trace_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Set the status of several incidents at once. Returns how many changed.
    pub async fn set_incidents_status(
        &self,
        ids: Vec<i64>,
        status: &'static str,
        note: String,
    ) -> Result<usize, String> {
        self.call(move |c| {
            let tx = c.unchecked_transaction()?;
            let mut n = 0;
            {
                let mut st = tx.prepare("UPDATE incidents SET status=?1, note=?2 WHERE id=?3")?;
                for id in ids {
                    n += st.execute(params![status, note, id])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })
        .await
    }

    /// Delete incidents and their traces. Returns how many incidents went.
    pub async fn delete_incidents(&self, ids: Vec<i64>) -> Result<usize, String> {
        self.call(move |c| {
            let tx = c.unchecked_transaction()?;
            let mut n = 0;
            {
                let mut traces = tx.prepare("DELETE FROM ai_traces WHERE incident_id=?1")?;
                let mut incidents = tx.prepare("DELETE FROM incidents WHERE id=?1")?;
                for id in ids {
                    traces.execute([id])?;
                    n += incidents.execute([id])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })
        .await
    }

    /// Incidents of the same kind on the same target opened after `since`
    /// (`YYYY-MM-DD HH:MM:SS`, UTC), and when the newest of them was opened.
    pub async fn similar_incidents(
        &self,
        target_id: i64,
        trigger: String,
        since: String,
    ) -> Result<(i64, Option<String>), String> {
        self.call(move |c| {
            c.query_row(
                "SELECT COUNT(*), MAX(created_at) FROM incidents WHERE target_id=?1 AND trigger=?2 AND created_at>=?3",
                params![target_id, trigger, since],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
    }

    pub async fn open_incident_count(&self) -> Result<i64, String> {
        self.call(|c| c.query_row("SELECT COUNT(*) FROM incidents WHERE status='open'", [], |r| r.get(0)))
            .await
    }

    pub async fn set_webhook_secret(&self, id: i64, secret: Option<String>) -> Result<(), String> {
        self.call(move |c| {
            let sealed = secret.map(|s| crate::secrets::seal(&s, WEBHOOK_SECRET_AAD));
            c.execute("UPDATE portal_targets SET webhook_secret=?1 WHERE id=?2", params![sealed, id])?;
            Ok(())
        })
        .await
    }

    pub async fn set_webhook_token(&self, id: i64, token: Option<String>) -> Result<(), String> {
        self.call(move |c| {
            c.execute("UPDATE portal_targets SET webhook_token=?1 WHERE id=?2", params![token, id])?;
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
        self.start_run_labeled(target_id, triggered_by, commit_sha, commit_message, None).await
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
                // The report can be megabytes: read it only when it is wanted.
                "SELECT id, target_id, critical_issues_found, vulnerable_packages_found, \
                 scanned_at, CASE WHEN ?2 THEN raw_payload_json END FROM security_scan_reports \
                 WHERE target_id=?1 ORDER BY id DESC LIMIT 1",
                params![target_id, with_payload],
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

    /// The newest scan of every target, without the report itself. One query
    /// for the whole target list instead of one per target.
    pub async fn latest_scan_summaries(&self) -> Result<std::collections::HashMap<i64, ScanReport>, String> {
        self.call(|c| {
            let mut st = c.prepare(
                "SELECT id, target_id, critical_issues_found, vulnerable_packages_found, scanned_at \
                 FROM security_scan_reports \
                 WHERE id IN (SELECT MAX(id) FROM security_scan_reports GROUP BY target_id)",
            )?;
            let rows = st.query_map([], |r| {
                Ok(ScanReport {
                    id: r.get(0)?,
                    target_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    critical_issues_found: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    vulnerable_packages_found: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    scanned_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    raw_payload: None,
                })
            })?;
            rows.map(|r| r.map(|s| (s.target_id, s))).collect()
        })
        .await
    }

    /// The newest run of every target (no captured output).
    pub async fn last_runs(&self) -> Result<std::collections::HashMap<i64, RunRecord>, String> {
        self.call(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {RUN_COLS} FROM process_execution_history h \
                 WHERE h.id IN (SELECT MAX(id) FROM process_execution_history GROUP BY target_id)"
            ))?;
            let rows = st.query_map([], |r| row_to_run(r, RunExtra::None))?;
            rows.map(|r| r.map(|run| (run.target_id, run))).collect()
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
    async fn secrets_are_encrypted_at_rest() {
        let db = Db::open_in_memory().unwrap();
        let mut t = input("a");
        t.env_vars = Some("DB_PASSWORD=hunter2".into());
        let id = db.insert_target(t).await.unwrap();
        db.set_webhook_secret(id, Some("whsec".into())).await.unwrap();

        let raw: (String, String) = db
            .call(move |c| {
                c.query_row("SELECT env_vars, webhook_secret FROM portal_targets WHERE id=?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
            })
            .await
            .unwrap();
        assert!(raw.0.starts_with("enc1:") && !raw.0.contains("hunter2"), "env stored sealed: {}", raw.0);
        assert!(raw.1.starts_with("enc1:") && !raw.1.contains("whsec"));

        let back = db.get_target(id).await.unwrap().unwrap();
        assert_eq!(back.env_vars, "DB_PASSWORD=hunter2");
        assert_eq!(back.webhook_secret.as_deref(), Some("whsec"));
    }

    #[tokio::test]
    async fn plaintext_from_before_is_sealed_at_start() {
        crate::secrets::ensure_test_key();
        let conn = Connection::open_in_memory().unwrap();
        let db = Db::init(conn).unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        db.call(move |c| c.execute("UPDATE portal_targets SET env_vars='OLD=1' WHERE id=?1", [id]))
            .await
            .unwrap();
        db.call(seal_plaintext_secrets).await.unwrap();
        let raw: String = db
            .call(move |c| c.query_row("SELECT env_vars FROM portal_targets WHERE id=?1", [id], |r| r.get(0)))
            .await
            .unwrap();
        assert!(crate::secrets::is_sealed(&raw));
        assert_eq!(db.get_target(id).await.unwrap().unwrap().env_vars, "OLD=1");
    }

    #[tokio::test]
    async fn unreadable_env_survives_a_save() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        // A value sealed under some other key: it cannot be opened here.
        let foreign = format!("enc1:{}", "ab".repeat(40));
        let stored = foreign.clone();
        db.call(move |c| c.execute("UPDATE portal_targets SET env_vars=?1 WHERE id=?2", params![stored, id]))
            .await
            .unwrap();
        assert_eq!(db.get_target(id).await.unwrap().unwrap().env_vars, "", "reads as empty");

        db.update_target(id, input("a")).await.unwrap(); // a save with empty variables
        let raw: String = db
            .call(move |c| c.query_row("SELECT env_vars FROM portal_targets WHERE id=?1", [id], |r| r.get(0)))
            .await
            .unwrap();
        assert_eq!(raw, foreign, "the undecryptable value is kept for when the key comes back");

        let mut set = input("a");
        set.env_vars = Some("NEW=1".into());
        db.update_target(id, set).await.unwrap(); // entering new values replaces it
        assert_eq!(db.get_target(id).await.unwrap().unwrap().env_vars, "NEW=1");
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
        let run = db.start_run(id, "MANUAL".into(), Some("abc123".into()), Some("fix".into())).await.unwrap();
        db.finish_run(run, "SUCCESS", Some(0), 12, "hello".into()).await.unwrap();
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
    async fn metric_points_round_trip_and_cascade() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        let point = |target_id, t| MetricPoint {
            target_id,
            t,
            cpu: Some(1.5),
            mem: Some(1_000),
            mem_max: Some(2_000),
            procs: Some(3),
            latency: None,
            probes: 4,
            probes_up: 3,
        };
        // The point for a missing target is skipped without failing the others.
        let stored = db
            .insert_metric_points(vec![point(id, 60), point(999, 60), point(id, 120), point(id, 120)])
            .await
            .unwrap();
        assert_eq!(stored, 3);
        let rows = db.metric_points_since(id, 100).await.unwrap();
        assert_eq!(rows, vec![point(id, 120)]);
        assert_eq!(db.metric_points_since(id, 0).await.unwrap().len(), 2);
        assert_eq!(db.prune_metric_points(100).await.unwrap(), 1);
        db.delete_target(id).await.unwrap();
        assert!(db.metric_points_since(id, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_invalid_git_status() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_target(input("a")).await.unwrap();
        assert!(db.set_git_status(id, "BOGUS").await.is_err());
    }

    #[tokio::test]
    async fn git_dir_is_stored_and_cleared() {
        let db = Db::open_in_memory().unwrap();
        let mut t = input("a");
        t.git_dir = Some("auto".into());
        let id = db.insert_target(t).await.unwrap();
        assert_eq!(db.get_target(id).await.unwrap().unwrap().git_dir.as_deref(), Some("auto"));
        let mut t = input("a");
        t.git_dir = Some(String::new());
        db.update_target(id, t).await.unwrap();
        assert_eq!(db.get_target(id).await.unwrap().unwrap().git_dir, None, "empty reads as not set");
    }

    #[tokio::test]
    async fn incidents_keep_details_and_change_in_bulk() {
        let db = Db::open_in_memory().unwrap();
        let tid = db.insert_target(input("a")).await.unwrap();
        let mut ids = Vec::new();
        for n in 0..3 {
            let inc = Incident {
                target_id: Some(tid),
                trigger: "failure".into(),
                severity: "warning".into(),
                title: format!("t{n}"),
                action: "none".into(),
                trace_id: format!("TR-{n}"),
                details: if n == 0 {
                    serde_json::json!({"history": {"same_24h": 2}})
                } else {
                    serde_json::Value::Null
                },
                ..Default::default()
            };
            ids.push(db.insert_incident(inc).await.unwrap());
            db.insert_trace(Trace {
                trace_id: format!("TR-{n}"),
                incident_id: ids.last().copied(),
                trigger: "failure".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let first = db.get_incident(ids[0]).await.unwrap().unwrap();
        assert_eq!(first.details["history"]["same_24h"], 2);
        assert!(db.get_incident(ids[1]).await.unwrap().unwrap().details.is_null(), "old rows read as null");

        let (n, last) =
            db.similar_incidents(tid, "failure".into(), "2000-01-01 00:00:00".into()).await.unwrap();
        assert_eq!(n, 3);
        assert!(last.is_some());
        assert_eq!(
            db.similar_incidents(tid, "attack".into(), "2000-01-01 00:00:00".into()).await.unwrap().0,
            0
        );

        assert_eq!(
            db.set_incidents_status(vec![ids[0], ids[1], 9999], "resolved", "bulk".into()).await.unwrap(),
            2
        );
        assert_eq!(db.list_incidents(true, 10).await.unwrap().len(), 1);
        assert_eq!(db.delete_incidents(vec![ids[0], ids[2]]).await.unwrap(), 2);
        assert_eq!(db.list_incidents(false, 10).await.unwrap().len(), 1);
        assert!(db.get_trace("TR-0".into()).await.unwrap().is_none(), "the trace goes with its incident");
        assert!(db.get_trace("TR-1".into()).await.unwrap().is_some());
    }
}
