//! Integrated security profiling plane (PRD module 4).
//!
//! One click runs two scanners against a target and stores the result:
//!   * static analysis:      `bandit -r <target_path> -f json`
//!   * dependency analysis:  `pip-audit --format json`
//! The JSON payloads are consumed here and reduced to the counters shown on
//! the dashboard cards; the raw payload is kept for the detail view.

use crate::db::Target;
use crate::runner::hide_window;
use crate::state::AppState;
use crate::util::log;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

const SCAN_TIMEOUT: Duration = Duration::from_secs(600);

/// Directories that hold third-party or generated code, not the app's own.
const BANDIT_EXCLUDES: &str = ".venv,venv,node_modules,.git,site-packages,__pycache__,staticfiles";

#[cfg(windows)]
const PYTHONS: &[&str] = &["python", "py"];
#[cfg(not(windows))]
const PYTHONS: &[&str] = &["python3", "python"];

struct ToolOutput {
    stdout: String,
    stderr: String,
}

enum ToolError {
    NotInstalled,
    Failed(String),
}

async fn capture(program: &str, args: &[String], dir: &Path) -> Result<ToolOutput, std::io::Error> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_window(&mut cmd);
    let child = cmd.spawn()?;
    // Dropping the future on timeout kills the child (kill_on_drop).
    match tokio::time::timeout(SCAN_TIMEOUT, child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok(ToolOutput {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out after {}s", SCAN_TIMEOUT.as_secs()),
        )),
    }
}

/// Run a Python CLI tool: first the console script on PATH, then
/// `python -m <module>` so a `pip install --user` without PATH also works.
async fn run_tool(exe: &str, module: &str, args: &[String], dir: &Path) -> Result<ToolOutput, ToolError> {
    match capture(exe, args, dir).await {
        Ok(out) => return Ok(out),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ToolError::Failed(format!("{exe}: {e}"))),
    }
    for python in PYTHONS {
        let mut full = vec!["-m".to_string(), module.to_string()];
        full.extend_from_slice(args);
        match capture(python, &full, dir).await {
            Ok(out) => {
                if out.stdout.trim().is_empty() && out.stderr.contains("No module named") {
                    continue;
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(ToolError::Failed(format!("{python} -m {module}: {e}"))),
        }
    }
    Err(ToolError::NotInstalled)
}

fn snippet(s: &str) -> String {
    let t = s.trim();
    let tail: String = t.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
    tail
}

/// Extract the JSON document from tool output (tolerates banner lines).
fn parse_json_output(stdout: &str) -> Option<Value> {
    let t = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    // Otherwise the document starts at the beginning of some later line.
    let mut offset = 0;
    for line in t.split_inclusive('\n') {
        if offset > 0 && line.starts_with(['{', '[']) {
            if let Ok(v) = serde_json::from_str::<Value>(&t[offset..]) {
                return Some(v);
            }
        }
        offset += line.len();
    }
    None
}

/// (high, medium, low) severity counts from a bandit JSON report.
pub fn bandit_counts(report: &Value) -> (i64, i64, i64) {
    let mut c = (0, 0, 0);
    if let Some(results) = report.get("results").and_then(Value::as_array) {
        for r in results {
            match r.get("issue_severity").and_then(Value::as_str).unwrap_or("") {
                "HIGH" => c.0 += 1,
                "MEDIUM" => c.1 += 1,
                "LOW" => c.2 += 1,
                _ => {}
            }
        }
    }
    c
}

/// Number of packages with at least one known vulnerability in a pip-audit report.
pub fn pip_audit_vulnerable(report: &Value) -> i64 {
    let deps = report.get("dependencies").and_then(Value::as_array).or_else(|| report.as_array());
    deps.map(|d| {
        d.iter()
            .filter(|p| p.get("vulns").and_then(Value::as_array).map(|v| !v.is_empty()).unwrap_or(false))
            .count() as i64
    })
    .unwrap_or(0)
}

fn find_site_packages(dir: &Path) -> Option<PathBuf> {
    for venv in [".venv", "venv", "env"] {
        let win = dir.join(venv).join("Lib").join("site-packages");
        if win.is_dir() {
            return Some(win);
        }
        if let Ok(entries) = std::fs::read_dir(dir.join(venv).join("lib")) {
            for e in entries.flatten() {
                let sp = e.path().join("site-packages");
                if sp.is_dir() {
                    return Some(sp);
                }
            }
        }
    }
    None
}

/// Decide what pip-audit should look at for this target:
/// the declared requirements, else the project's virtualenv, else the
/// ambient Python environment.
pub fn pip_audit_plan(dir: &Path) -> (String, Vec<String>) {
    let mut args: Vec<String> =
        ["--format", "json", "--progress-spinner", "off"].iter().map(|s| s.to_string()).collect();
    if dir.join("requirements.txt").is_file() {
        args.extend(["-r".to_string(), "requirements.txt".to_string()]);
        ("requirements.txt".into(), args)
    } else if let Some(sp) = find_site_packages(dir) {
        args.extend(["--path".to_string(), sp.to_string_lossy().into_owned()]);
        (format!("virtualenv ({})", sp.display()), args)
    } else {
        ("ambient Python environment".into(), args)
    }
}

async fn run_bandit(dir: &Path) -> Result<Value, String> {
    let args: Vec<String> = vec![
        "-r".into(),
        dir.to_string_lossy().into_owned(),
        "-f".into(),
        "json".into(),
        "-q".into(),
        "-x".into(),
        BANDIT_EXCLUDES.into(),
    ];
    match run_tool("bandit", "bandit", &args, dir).await {
        Ok(out) => parse_json_output(&out.stdout)
            .ok_or_else(|| format!("bandit produced no JSON report: {}", snippet(&out.stderr))),
        Err(ToolError::NotInstalled) => Err("bandit is not installed (run: pip install bandit)".into()),
        Err(ToolError::Failed(e)) => Err(e),
    }
}

async fn run_pip_audit(dir: &Path, args: &[String]) -> Result<Value, String> {
    match run_tool("pip-audit", "pip_audit", args, dir).await {
        Ok(out) => parse_json_output(&out.stdout)
            .ok_or_else(|| format!("pip-audit produced no JSON report: {}", snippet(&out.stderr))),
        Err(ToolError::NotInstalled) => Err("pip-audit is not installed (run: pip install pip-audit)".into()),
        Err(ToolError::Failed(e)) => Err(e),
    }
}

/// Mark the target as being scanned and run both scanners in the background.
pub fn start_scan(state: &Arc<AppState>, target: Target) -> Result<(), String> {
    let dir = PathBuf::from(&target.working_directory);
    if !dir.is_dir() {
        return Err(format!("working directory does not exist: {}", target.working_directory));
    }
    {
        let mut set = state.scans_running.lock().unwrap_or_else(|p| p.into_inner());
        if !set.insert(target.id) {
            return Err("a scan is already running for this target".into());
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        let id = target.id;
        let say = |msg: String| state.log_line(id, 0, "system", &format!("[scan] {msg}"), None);
        state.emit(json!({"type": "scan_started", "target_id": id}));
        log(&format!("security scan started for target {id} ({})", target.name));

        let (mode, audit_args) = pip_audit_plan(&dir);
        say(format!("bandit -r \"{}\" -f json", dir.display()));
        say(format!("pip-audit --format json  (auditing {mode})"));

        let (bandit, audit) = tokio::join!(run_bandit(&dir), run_pip_audit(&dir, &audit_args));

        let (high, medium, low) = bandit.as_ref().map(bandit_counts).unwrap_or((0, 0, 0));
        let vulnerable = audit.as_ref().map(pip_audit_vulnerable).unwrap_or(0);
        match &bandit {
            Ok(_) => say(format!("bandit: {high} high, {medium} medium, {low} low severity issue(s)")),
            Err(e) => say(format!("bandit failed: {e}")),
        }
        match &audit {
            Ok(_) => say(format!("pip-audit: {vulnerable} vulnerable package(s)")),
            Err(e) => say(format!("pip-audit failed: {e}")),
        }

        let payload = json!({
            "bandit": bandit.as_ref().ok(),
            "bandit_error": bandit.as_ref().err(),
            "bandit_severity": {"high": high, "medium": medium, "low": low},
            "pip_audit": audit.as_ref().ok(),
            "pip_audit_error": audit.as_ref().err(),
            "pip_audit_mode": mode,
        });
        let stored = state.db.insert_scan(id, high, vulnerable, payload.to_string()).await;
        if let Err(e) = &stored {
            log(&format!("failed to store scan report for target {id}: {e}"));
        }
        state.scans_running.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        state.emit(json!({
            "type": "scan_finished",
            "target_id": id,
            "critical_issues_found": high,
            "vulnerable_packages_found": vulnerable,
            "ok": bandit.is_ok() && audit.is_ok() && stored.is_ok(),
        }));
        state.activity(
            if high + vulnerable > 0 { "warn" } else { "ok" },
            "scan",
            Some(id),
            format!("Security scan finished: {high} critical issue(s), {vulnerable} vulnerable package(s)"),
        );
        log(&format!(
            "security scan finished for target {id}: {high} critical, {vulnerable} vulnerable packages"
        ));
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bandit_report() {
        let report = json!({
            "errors": [],
            "metrics": {"_totals": {"SEVERITY.HIGH": 2}},
            "results": [
                {"issue_severity": "HIGH", "test_id": "B602", "filename": "a.py", "line_number": 3},
                {"issue_severity": "HIGH", "test_id": "B307", "filename": "b.py", "line_number": 9},
                {"issue_severity": "MEDIUM", "test_id": "B104", "filename": "c.py", "line_number": 1},
                {"issue_severity": "LOW", "test_id": "B101", "filename": "d.py", "line_number": 1}
            ]
        });
        assert_eq!(bandit_counts(&report), (2, 1, 1));
        assert_eq!(bandit_counts(&json!({"results": []})), (0, 0, 0));
        assert_eq!(bandit_counts(&json!({})), (0, 0, 0));
    }

    #[test]
    fn parses_pip_audit_report_in_both_shapes() {
        let modern = json!({
            "dependencies": [
                {"name": "django", "version": "4.2.0", "vulns": [{"id": "PYSEC-1", "fix_versions": ["4.2.1"]}]},
                {"name": "requests", "version": "2.31.0", "vulns": []},
                {"name": "pillow", "version": "9.0.0", "vulns": [{"id": "A"}, {"id": "B"}]},
                {"name": "local-pkg", "skip_reason": "not on PyPI"}
            ],
            "fixes": []
        });
        assert_eq!(pip_audit_vulnerable(&modern), 2);
        let legacy = json!([{"name": "x", "version": "1", "vulns": [{"id": "A"}]}]);
        assert_eq!(pip_audit_vulnerable(&legacy), 1);
        assert_eq!(pip_audit_vulnerable(&json!({})), 0);
    }

    #[test]
    fn json_extraction_tolerates_banners() {
        assert!(parse_json_output("{\"results\": []}").is_some());
        assert!(parse_json_output("[main] INFO running\n{\"results\": []}").is_some());
        assert!(parse_json_output("no json here").is_none());
        assert!(parse_json_output("").is_none());
    }

    #[test]
    fn audit_plan_prefers_requirements_then_virtualenv() {
        let dir = tempfile::tempdir().unwrap();
        let (mode, args) = pip_audit_plan(dir.path());
        assert_eq!(mode, "ambient Python environment");
        assert!(!args.contains(&"-r".to_string()));

        let sp = dir.path().join(".venv").join("lib").join("python3.10").join("site-packages");
        std::fs::create_dir_all(&sp).unwrap();
        let (mode, args) = pip_audit_plan(dir.path());
        assert!(mode.starts_with("virtualenv"));
        assert!(args.contains(&"--path".to_string()));

        std::fs::write(dir.path().join("requirements.txt"), "django\n").unwrap();
        let (mode, args) = pip_audit_plan(dir.path());
        assert_eq!(mode, "requirements.txt");
        assert!(args.ends_with(&["-r".to_string(), "requirements.txt".to_string()]));
    }
}
