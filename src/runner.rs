//! OS-aware asynchronous process worker (PRD module 3).
//!
//! Scripts run through the host's native shell, bound to the target's working
//! directory. Output is streamed line by line to WebSocket clients; nothing
//! is buffered beyond a small bounded tail kept for history.

use crate::db::Target;
use crate::state::{AppState, LogTail, RunHandle, MAX_LINE_BYTES};
use crate::util::log;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

/// Compile-time shell routing, exactly as specified in the PRD.
pub fn shell() -> (&'static str, &'static str) {
    let (shell_binary, shell_switch) = if cfg!(target_os = "windows") {
        ("cmd.exe", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    (shell_binary, shell_switch)
}

/// Stop spawned console programs from popping up a window when the manager
/// itself runs without a console (detached / started at login).
pub fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// A script name must be a plain relative path inside the working directory:
/// no traversal, no absolute paths, no shell metacharacters.
pub fn validate_script_name(script: &str) -> Result<(), String> {
    let s = script.trim();
    if s.is_empty() {
        return Err("script name is required".into());
    }
    if s != script {
        return Err("script name must not start or end with whitespace".into());
    }
    if s.len() > 260 {
        return Err("script name is too long".into());
    }
    const FORBIDDEN: &[char] = &[
        '"', '\'', '&', '|', '<', '>', '^', '%', '!', ';', '`', '$', '*', '?', '\n', '\r', '\0',
        '(', ')', ',', '=',
    ];
    if let Some(c) = s.chars().find(|c| FORBIDDEN.contains(c) || c.is_control()) {
        return Err(format!("script name contains a forbidden character: {c:?}"));
    }
    if s.starts_with('/') || s.starts_with('\\') || s.starts_with('-') || s.contains(':') {
        return Err("script name must be a relative path inside the working directory".into());
    }
    if s.split(['/', '\\']).any(|part| part == ".." || part.is_empty()) {
        return Err("script name must not contain '..' or empty path segments".into());
    }
    Ok(())
}

/// Validate the directory + script pair and return the directory path.
pub fn validate_target_paths(working_directory: &str, script: &str) -> Result<PathBuf, String> {
    let dir = PathBuf::from(working_directory);
    if !dir.is_absolute() {
        return Err("working directory must be an absolute path".into());
    }
    if !dir.is_dir() {
        return Err(format!("working directory does not exist: {working_directory}"));
    }
    validate_script_name(script)?;
    if !dir.join(script).is_file() {
        return Err(format!("script '{script}' was not found in {working_directory}"));
    }
    Ok(dir)
}

/// Parse a `.env`-style block: one `KEY=VALUE` per line, `#` comments and
/// blank lines ignored, an optional `export ` prefix and optional quotes
/// around the value.
pub fn parse_env(text: &str) -> Result<Vec<(String, String)>, String> {
    if text.len() > 64 * 1024 {
        return Err("environment variables are limited to 64 KB".into());
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected KEY=VALUE", n + 1))?;
        let key = key.trim();
        let valid = !key.is_empty()
            && key.len() <= 128
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!(
                "line {}: '{key}' is not a valid variable name (letters, digits and _ only)",
                n + 1
            ));
        }
        let value = value.trim();
        let value = match (value.chars().next(), value.chars().last()) {
            (Some('"'), Some('"')) | (Some('\''), Some('\'')) if value.len() >= 2 => {
                &value[1..value.len() - 1]
            }
            _ => value,
        };
        if value.contains('\0') {
            return Err(format!("line {}: value contains a NUL byte", n + 1));
        }
        out.retain(|(k, _)| k != key);
        out.push((key.to_string(), value.to_string()));
    }
    if out.len() > 200 {
        return Err("at most 200 environment variables are supported".into());
    }
    Ok(out)
}

fn build_command(dir: &Path, script: &str, env: &[(String, String)]) -> Command {
    let (shell_binary, shell_switch) = shell();
    let mut cmd = Command::new(shell_binary);
    #[cfg(windows)]
    {
        // /D skips AutoRun, /S makes cmd strip exactly the outer quote pair,
        // leaving the script path quoted so spaces are safe. The explicit
        // `.\` matters: with NoDefaultCurrentDirectoryInExePath set, cmd does
        // not search the current directory for a bare script name.
        let script = script.replace('/', "\\");
        cmd.arg("/D").arg("/S").arg(shell_switch);
        cmd.raw_arg(format!("\"\".\\{script}\"\""));
    }
    #[cfg(not(windows))]
    {
        let rel = format!("./{}", script.replace('\\', "/"));
        cmd.arg(shell_switch)
            .arg("if [ -x \"$0\" ]; then exec \"$0\"; else exec /bin/sh \"$0\"; fi")
            .arg(rel);
        // Own process group so the whole tree can be signalled.
        cmd.process_group(0);
    }
    hide_window(&mut cmd);
    cmd.current_dir(dir)
        .env("PYTHONUNBUFFERED", "1")
        .env("PORTALCONTROL", "1")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Terminate a process and all of its descendants.
pub async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_window(&mut c);
        let _ = c.status().await;
    }
    #[cfg(not(windows))]
    {
        let pgid = pid as i32;
        // SAFETY: plain signal delivery to the process group we created.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        tokio::time::sleep(Duration::from_millis(1500)).await;
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

async fn pump<R: AsyncRead + Unpin>(
    reader: R,
    stream: &'static str,
    state: Arc<AppState>,
    target_id: i64,
    run_id: i64,
    tail: Arc<Mutex<LogTail>>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        buf.clear();
        // `take` caps a single line, so a script that never prints a newline
        // cannot grow this buffer without bound.
        let n = (&mut reader)
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut buf)
            .await;
        match n {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim_end_matches(['\r', '\n']);
        state.log_line(target_id, run_id, stream, line, Some(&tail));
    }
}

async fn sleep_opt(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}

enum Outcome {
    Exited(Option<i64>),
    Stopped,
    TimedOut,
    WaitFailed(String),
}

/// What `launch` supervises: the target's script, or a one-off command.
struct Job {
    cmd: Command,
    /// Key in `AppState::running`: the target id for its script, the negated
    /// id for a one-off command (so both can be active at the same time).
    slot: i64,
    /// Display name of a one-off command; `None` for the script.
    label: Option<String>,
    /// The command line as shown in the log.
    describe: String,
    env_names: Vec<String>,
    timeout: Option<Duration>,
    is_service: bool,
    auto_restart: bool,
}

/// Slot of a target's one-off command in `AppState::running`.
pub fn command_slot(target_id: i64) -> i64 {
    -target_id
}

/// How long a one-off command may run before it is terminated.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(900);

/// Start the target's script. Returns the execution-history row id.
pub async fn start_run(
    state: &Arc<AppState>,
    target: &Target,
    triggered_by: &str,
) -> Result<i64, String> {
    let dir = validate_target_paths(&target.working_directory, &target.script_name)?;
    let env = parse_env(&target.env_vars).map_err(|e| format!("environment variables: {e}"))?;
    let is_service = target.run_mode == "SERVICE";
    if target.backup_on_deploy && triggered_by != "AUTO_RESTART" {
        crate::backup::before_run(state, target).await;
    }
    let job = Job {
        cmd: build_command(&dir, &target.script_name, &env),
        slot: target.id,
        label: None,
        describe: format!("{} {} \"{}\"", shell().0, shell().1, target.script_name),
        env_names: env.into_iter().map(|(k, _)| k).collect(),
        timeout: if is_service {
            None
        } else {
            Some(Duration::from_secs(target.timeout_secs.max(1)))
        },
        is_service,
        auto_restart: is_service && target.auto_restart,
    };
    launch(state, target, triggered_by, job).await
}

/// Run a one-off command (a fixed program + arguments, no shell) in the
/// target's working directory. Its output goes to the target's log stream
/// and it is recorded in the history with `label`.
pub async fn start_command(
    state: &Arc<AppState>,
    target: &Target,
    label: &str,
    program: &str,
    args: &[String],
) -> Result<i64, String> {
    let dir = PathBuf::from(&target.working_directory);
    if !dir.is_dir() {
        return Err(format!("working directory does not exist: {}", target.working_directory));
    }
    let env = parse_env(&target.env_vars).map_err(|e| format!("environment variables: {e}"))?;
    // A relative program path (the project's virtualenv) is resolved against the directory.
    let executable = if program.contains(['/', '\\']) {
        dir.join(program)
    } else {
        PathBuf::from(program)
    };
    let mut cmd = Command::new(executable);
    cmd.args(args);
    #[cfg(not(windows))]
    cmd.process_group(0);
    hide_window(&mut cmd);
    cmd.current_dir(&dir)
        .env("PYTHONUNBUFFERED", "1")
        .env("PORTALCONTROL", "1")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let job = Job {
        cmd,
        slot: command_slot(target.id),
        label: Some(label.to_string()),
        describe: format!("{program} {}", args.join(" ")),
        env_names: env.into_iter().map(|(k, _)| k).collect(),
        timeout: Some(COMMAND_TIMEOUT),
        is_service: false,
        auto_restart: false,
    };
    launch(state, target, "COMMAND", job).await
}

async fn launch(
    state: &Arc<AppState>,
    target: &Target,
    triggered_by: &str,
    job: Job,
) -> Result<i64, String> {
    let Job { mut cmd, slot, label, describe, env_names, timeout, is_service, auto_restart } = job;
    let what: String = match &label {
        Some(l) => format!("Command \"{l}\""),
        None if is_service => "Service run".into(),
        None => "Deployment".into(),
    };
    // The commit being deployed, as last seen by the Git worker.
    let (commit_sha, commit_message) = state
        .git
        .lock()
        .ok()
        .and_then(|g| g.get(&target.id).map(|i| (i.head.clone(), i.head_summary.clone())))
        .unwrap_or((None, None));

    // Held across the spawn so two requests cannot start the same target twice.
    let mut running = state.running.lock().await;
    if running.contains_key(&slot) {
        return Err(if label.is_some() {
            "a command is already running for this target".into()
        } else {
            "target is already running".into()
        });
    }

    let run_id = state
        .db
        .start_run_labeled(
            target.id,
            triggered_by.to_string(),
            commit_sha,
            commit_message,
            label.clone(),
        )
        .await?;
    let started = Instant::now();

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to start {describe}: {e}");
            let _ = state
                .db
                .finish_run(run_id, "FAILED", None, 0, format!("[portalcontrol] {msg}\n"))
                .await;
            return Err(msg);
        }
    };

    let pid = child.id();
    let cancel = Arc::new(Notify::new());
    let tail = Arc::new(Mutex::new(LogTail::default()));
    running.insert(
        slot,
        RunHandle {
            run_id,
            pid,
            started,
            started_ms: crate::util::now_ms(),
            triggered_by: triggered_by.to_string(),
            cancel: cancel.clone(),
            tail: tail.clone(),
        },
    );
    drop(running);

    let target_id = target.id;
    state.emit(json!({"type": "run_started", "target_id": target_id, "run_id": run_id, "pid": pid}));
    state.activity(
        "info",
        "run",
        Some(target_id),
        format!("{what} #{run_id} started ({})", trigger_label(triggered_by)),
    );
    if !env_names.is_empty() {
        state.log_line(
            target_id,
            run_id,
            "system",
            &format!(
                "[portalcontrol] {} environment variable(s) set: {}",
                env_names.len(),
                env_names.join(", ")
            ),
            Some(&tail),
        );
    }
    state.log_line(
        target_id,
        run_id,
        "system",
        &format!(
            "[portalcontrol] {} in {} (pid {}, {})",
            describe,
            target.working_directory,
            pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            match timeout {
                Some(t) => format!("timeout {}s", t.as_secs()),
                None => "service, no timeout".into(),
            }
        ),
        Some(&tail),
    );
    log(&format!(
        "run {run_id} started for target {target_id} ({}) by {triggered_by}",
        target.name
    ));

    // Don't make the dashboard wait for the next periodic probe: check health
    // a few times while the process comes up, and once more when it ends.
    let health_url = target
        .health_url
        .clone()
        .filter(|u| !u.trim().is_empty() && label.is_none());
    if let Some(url) = health_url.clone() {
        let st = state.clone();
        tokio::spawn(async move {
            for delay_secs in [1u64, 2, 3, 4, 5] {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                let still_this_run = st
                    .running
                    .lock()
                    .await
                    .get(&slot)
                    .map(|h| h.run_id == run_id)
                    .unwrap_or(false);
                if !still_this_run {
                    break;
                }
                if crate::health::probe_target(&st, target_id, &url).await.status == "LIVE" {
                    break;
                }
            }
        });
    }

    let state = state.clone();
    tokio::spawn(async move {
        let out = child.stdout.take();
        let err = child.stderr.take();
        let t_out = out.map(|o| {
            tokio::spawn(pump(o, "stdout", state.clone(), target_id, run_id, tail.clone()))
        });
        let t_err = err.map(|e| {
            tokio::spawn(pump(e, "stderr", state.clone(), target_id, run_id, tail.clone()))
        });

        let outcome = tokio::select! {
            r = child.wait() => match r {
                Ok(st) => Outcome::Exited(st.code().map(|c| c as i64)),
                Err(e) => Outcome::WaitFailed(e.to_string()),
            },
            _ = cancel.notified() => Outcome::Stopped,
            _ = sleep_opt(timeout) => Outcome::TimedOut,
        };

        if !matches!(outcome, Outcome::Exited(_)) {
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.kill().await;
        }

        // Let the readers drain what is left; a stray grandchild that still
        // holds the pipe must not keep this run open forever.
        let aborts: Vec<_> = [&t_out, &t_err]
            .into_iter()
            .flatten()
            .map(|t| t.abort_handle())
            .collect();
        let drained = tokio::time::timeout(Duration::from_secs(3), async {
            if let Some(t) = t_out {
                let _ = t.await;
            }
            if let Some(t) = t_err {
                let _ = t.await;
            }
        })
        .await;
        if drained.is_err() {
            aborts.iter().for_each(|a| a.abort());
        }

        // A service that ends on its own (it was not stopped) has crashed.
        let unexpected_exit = matches!(outcome, Outcome::Exited(_) | Outcome::WaitFailed(_));
        let (status, exit_code, note): (&'static str, Option<i64>, String) = match outcome {
            Outcome::Exited(code) => (
                if code == Some(0) { "SUCCESS" } else { "FAILED" },
                code,
                format!(
                    "[portalcontrol] process exited with code {}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into())
                ),
            ),
            Outcome::Stopped => (
                if is_service { "SUCCESS" } else { "FAILED" },
                None,
                "[portalcontrol] stopped by operator; process tree terminated".into(),
            ),
            Outcome::TimedOut => (
                "FAILED",
                None,
                format!(
                    "[portalcontrol] timeout after {}s; process tree terminated",
                    timeout.map(|t| t.as_secs()).unwrap_or(0)
                ),
            ),
            Outcome::WaitFailed(e) => ("FAILED", None, format!("[portalcontrol] wait failed: {e}")),
        };
        state.log_line(target_id, run_id, "system", &note, Some(&tail));

        let duration_ms = started.elapsed().as_millis() as i64;
        let logs = tail.lock().map(|t| t.joined()).unwrap_or_default();
        if let Err(e) = state
            .db
            .finish_run(run_id, status, exit_code, duration_ms, logs)
            .await
        {
            log(&format!("failed to persist run {run_id}: {e}"));
        }
        state.running.lock().await.remove(&slot);
        state.emit(json!({
            "type": "run_finished",
            "target_id": target_id,
            "run_id": run_id,
            "status": status,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
        }));
        log(&format!("run {run_id} finished: {status} in {duration_ms}ms"));
        state.activity(
            if status == "SUCCESS" { "ok" } else { "error" },
            "run",
            Some(target_id),
            format!(
                "{what} #{run_id} {} after {}{}",
                if status == "SUCCESS" { "finished" } else { "failed" },
                fmt_duration(duration_ms),
                exit_code.map(|c| format!(" (exit code {c})")).unwrap_or_default()
            ),
        );
        let _ = state.db.prune_runs(target_id).await;
        if auto_restart && unexpected_exit {
            queue_restart(&state, target_id);
        }
        if let Some(url) = health_url {
            crate::health::probe_target(&state, target_id, &url).await;
        }
    });

    Ok(run_id)
}

/// Human wording for the `triggered_by` column.
pub fn trigger_label(trigger: &str) -> &str {
    match trigger {
        "MANUAL" => "started manually",
        "SYSTEM_AUTO" => "autostart",
        "SCHEDULE" => "scheduled",
        "GIT_AUTO" => "auto-deploy on new commits",
        "WEBHOOK" => "webhook",
        "AUTO_RESTART" => "automatic restart after a crash",
        "COMMAND" => "quick command",
        "ROLLBACK" => "rollback",
        "RESTART" => "restart",
        other => other,
    }
}

fn fmt_duration(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Automatic restarts allowed inside `RESTART_WINDOW` before giving up.
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Schedule a crashed service for restart, unless it is crash-looping. The
/// automation worker performs the restart, which keeps this task from
/// having to start itself recursively.
fn queue_restart(state: &Arc<AppState>, target_id: i64) {
    let now = Instant::now();
    let recent = {
        let mut log = state.restart_log.lock().unwrap_or_else(|p| p.into_inner());
        let entry = log.entry(target_id).or_default();
        entry.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        if entry.len() < MAX_RESTARTS {
            entry.push(now);
        }
        entry.len()
    };
    let already_queued = {
        let queue = state.restart_queue.lock().unwrap_or_else(|p| p.into_inner());
        queue.iter().any(|(id, _)| *id == target_id)
    };
    if already_queued {
        return;
    }
    if recent >= MAX_RESTARTS {
        state.activity(
            "error",
            "restart",
            Some(target_id),
            format!(
                "Service crashed {MAX_RESTARTS} times in {} minutes; automatic restart suspended",
                RESTART_WINDOW.as_secs() / 60
            ),
        );
        return;
    }
    state
        .restart_queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((target_id, now + RESTART_DELAY));
    state.activity(
        "warn",
        "restart",
        Some(target_id),
        format!(
            "Service exited unexpectedly; restarting in {} s (attempt {recent} of {})",
            RESTART_DELAY.as_secs(),
            MAX_RESTARTS - 1
        ),
    );
}

/// Stop the target if it is running, wait for the process tree to go away,
/// then start it again.
pub async fn restart(
    state: &Arc<AppState>,
    target: &Target,
    triggered_by: &str,
) -> Result<i64, String> {
    if stop_run(state, target.id).await {
        let mut gone = false;
        for _ in 0..150 {
            if !state.running.lock().await.contains_key(&target.id) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !gone {
            return Err("the running process did not stop within 15 seconds".into());
        }
    }
    start_run(state, target, triggered_by).await
}

/// Ask the supervisor task of a running target to terminate it.
pub async fn stop_run(state: &Arc<AppState>, target_id: i64) -> bool {
    match state.running.lock().await.get(&target_id) {
        Some(h) => {
            h.cancel.notify_one();
            true
        }
        None => false,
    }
}

/// Stop everything and wait (bounded) until the children are gone.
pub async fn stop_all(state: &Arc<AppState>) {
    let ids: Vec<i64> = state.running.lock().await.keys().copied().collect();
    if ids.is_empty() {
        return;
    }
    for id in &ids {
        stop_run(state, *id).await;
    }
    for _ in 0..80 {
        if state.running.lock().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, TargetInput};

    #[test]
    fn shell_matches_platform() {
        let (bin, switch) = shell();
        if cfg!(target_os = "windows") {
            assert_eq!((bin, switch), ("cmd.exe", "/C"));
        } else {
            assert_eq!((bin, switch), ("/bin/sh", "-c"));
        }
    }

    #[test]
    fn script_name_rules() {
        for ok in ["deploy.bat", "deploy.sh", "scripts/deploy.sh", "scripts\\deploy.bat", "my deploy.bat"] {
            assert!(validate_script_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", " deploy.bat", "../deploy.bat", "a/../b.bat", "/etc/passwd", "C:\\x.bat",
            "a.bat & calc", "a.bat|b", "\"a.bat\"", "a%PATH%.bat", "a;b", "$(x).sh", "-rf",
            "a//b.sh",
        ] {
            assert!(validate_script_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    fn script(dir: &Path, name: &str, win: &str, unix: &str) -> String {
        let (file, body) = if cfg!(windows) {
            (format!("{name}.bat"), format!("@echo off\r\n{win}\r\n"))
        } else {
            (format!("{name}.sh"), format!("#!/bin/sh\n{unix}\n"))
        };
        std::fs::write(dir.join(&file), body).unwrap();
        file
    }

    async fn setup(
        dir: &Path,
        script_name: &str,
        mode: &str,
        timeout: u64,
    ) -> (Arc<AppState>, Target) {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .insert_target(TargetInput {
                name: "t".into(),
                working_directory: dir.to_string_lossy().into_owned(),
                script_name: script_name.into(),
                run_mode: Some(mode.into()),
                timeout_secs: Some(timeout),
                ..Default::default()
            })
            .await
            .unwrap();
        let target = db.get_target(id).await.unwrap().unwrap();
        let state = AppState::new(db, "127.0.0.1:0".parse().unwrap());
        (state, target)
    }

    async fn wait_done(state: &Arc<AppState>, run_id: i64) -> crate::db::RunRecord {
        for _ in 0..300 {
            let r = state.db.get_run(run_id).await.unwrap().unwrap();
            if r.status != "RUNNING" {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("run {run_id} never finished");
    }

    #[tokio::test]
    async fn runs_script_in_working_directory_and_streams_lines() {
        // A directory name with a space exercises the quoting path.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("my portal");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let name = script(
            &dir,
            "deploy",
            "echo hello-from-script\r\nif exist marker.txt echo cwd-ok\r\necho to-stderr 1>&2",
            "echo hello-from-script\n[ -f marker.txt ] && echo cwd-ok\necho to-stderr 1>&2",
        );
        let (state, target) = setup(&dir, &name, "TASK", 30).await;
        let mut rx = state.events.subscribe();

        let run_id = start_run(&state, &target, "MANUAL").await.unwrap();
        let rec = wait_done(&state, run_id).await;
        assert_eq!(rec.status, "SUCCESS");
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.triggered_by, "MANUAL");
        let logs = rec.captured_logs.unwrap();
        assert!(logs.contains("hello-from-script"), "{logs}");
        assert!(logs.contains("cwd-ok"), "{logs}");
        assert!(logs.contains("to-stderr"), "{logs}");

        let mut streamed = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            streamed.push(ev);
        }
        assert!(streamed.iter().any(|e| e.contains("\"type\":\"log\"") && e.contains("hello-from-script")));
        assert!(streamed.iter().any(|e| e.contains("\"stream\":\"stderr\"") && e.contains("to-stderr")));
        assert!(streamed.iter().any(|e| e.contains("\"type\":\"run_finished\"")));
        assert!(state.running.lock().await.is_empty());
    }

    #[test]
    fn env_block_parsing() {
        let env = parse_env(
            "# comment\n\nDEBUG=1\nexport  SECRET_KEY=\"a b=c\"\nEMPTY=\nNAME='x'\nDEBUG=2\n",
        )
        .unwrap();
        assert_eq!(
            env,
            vec![
                ("SECRET_KEY".to_string(), "a b=c".to_string()),
                ("EMPTY".to_string(), String::new()),
                ("NAME".to_string(), "x".to_string()),
                ("DEBUG".to_string(), "2".to_string()),
            ]
        );
        assert!(parse_env("").unwrap().is_empty());
        for bad in ["NOVALUE", "1ABC=x", "A-B=x", "=x", "A B=x"] {
            assert!(parse_env(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn environment_variables_reach_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(
            dir.path(),
            "env",
            "echo value=%PC_TEST_VALUE%",
            "echo value=$PC_TEST_VALUE",
        );
        let (state, mut target) = setup(dir.path(), &name, "TASK", 30).await;
        target.env_vars = "PC_TEST_VALUE=hello world".into();
        let run_id = start_run(&state, &target, "SCHEDULE").await.unwrap();
        let rec = wait_done(&state, run_id).await;
        let logs = rec.captured_logs.unwrap();
        assert!(logs.contains("value=hello world"), "{logs}");
        assert!(logs.contains("1 environment variable(s) set: PC_TEST_VALUE"), "{logs}");
        assert_eq!(rec.triggered_by, "SCHEDULE");
        let feed = state.activity.lock().unwrap();
        assert!(feed.iter().any(|e| e["message"].as_str().unwrap().contains("started (scheduled)")));
        assert!(feed.iter().any(|e| e["level"] == "ok" && e["kind"] == "run"));
    }

    #[tokio::test]
    async fn crashed_service_is_queued_for_restart_and_restart_works() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(dir.path(), "crash", "echo dying\r\nexit /b 1", "echo dying\nexit 1");
        let (state, mut target) = setup(dir.path(), &name, "SERVICE", 30).await;
        target.auto_restart = true;
        let run_id = start_run(&state, &target, "MANUAL").await.unwrap();
        assert_eq!(wait_done(&state, run_id).await.status, "FAILED");
        for _ in 0..50 {
            if !state.restart_queue.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(state.restart_queue.lock().unwrap()[0].0, target.id);

        // An explicit restart of an idle target simply starts it.
        target.auto_restart = false;
        let again = restart(&state, &target, "RESTART").await.unwrap();
        assert!(again > run_id);
        assert_eq!(wait_done(&state, again).await.triggered_by, "RESTART");
    }

    #[tokio::test]
    async fn one_off_command_runs_beside_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(
            dir.path(),
            "serve",
            "echo serving\r\nping -n 60 127.0.0.1 >nul",
            "echo serving\nsleep 60",
        );
        let (state, mut target) = setup(dir.path(), &name, "SERVICE", 30).await;
        target.env_vars = "PC_CMD_VALUE=from-env".into();
        let service_run = start_run(&state, &target, "MANUAL").await.unwrap();

        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            ("cmd.exe", vec!["/C".into(), "echo command-output %PC_CMD_VALUE%".into()])
        } else {
            ("/bin/sh", vec!["-c".into(), "echo command-output $PC_CMD_VALUE".into()])
        };
        let cmd_run = start_command(&state, &target, "Say hello", program, &args).await.unwrap();
        let rec = wait_done(&state, cmd_run).await;
        assert_eq!(rec.status, "SUCCESS");
        assert_eq!(rec.triggered_by, "COMMAND");
        assert_eq!(rec.label.as_deref(), Some("Say hello"));
        assert!(rec.captured_logs.unwrap().contains("command-output from-env"));

        // The service was not disturbed, and its own history row has no label.
        assert!(state.running.lock().await.contains_key(&target.id));
        assert!(!state.running.lock().await.contains_key(&command_slot(target.id)));
        assert!(stop_run(&state, target.id).await);
        assert!(wait_done(&state, service_run).await.label.is_none());

        // A program that does not exist is reported, not left RUNNING.
        let err = start_command(&state, &target, "Broken", "no-such-program-pc", &[]).await;
        assert!(err.unwrap_err().contains("failed to start"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_failed() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(dir.path(), "bad", "echo boom\r\nexit /b 3", "echo boom\nexit 3");
        let (state, target) = setup(dir.path(), &name, "TASK", 30).await;
        let run_id = start_run(&state, &target, "MANUAL").await.unwrap();
        let rec = wait_done(&state, run_id).await;
        assert_eq!(rec.status, "FAILED");
        assert_eq!(rec.exit_code, Some(3));
    }

    #[tokio::test]
    async fn blocked_script_is_killed_at_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(
            dir.path(),
            "hang",
            "echo waiting\r\nping -n 60 127.0.0.1 >nul",
            "echo waiting\nsleep 60",
        );
        let (state, target) = setup(dir.path(), &name, "TASK", 2).await;
        let t0 = Instant::now();
        let run_id = start_run(&state, &target, "SYSTEM_AUTO").await.unwrap();
        let rec = wait_done(&state, run_id).await;
        assert_eq!(rec.status, "FAILED");
        assert!(t0.elapsed() < Duration::from_secs(20), "timeout was not enforced");
        assert!(rec.captured_logs.unwrap().contains("timeout after 2s"));
    }

    #[tokio::test]
    async fn service_runs_until_stopped_and_rejects_double_start() {
        let dir = tempfile::tempdir().unwrap();
        let name = script(
            dir.path(),
            "serve",
            "echo serving\r\nping -n 60 127.0.0.1 >nul",
            "echo serving\nsleep 60",
        );
        let (state, target) = setup(dir.path(), &name, "SERVICE", 1).await;
        let run_id = start_run(&state, &target, "MANUAL").await.unwrap();
        // Longer than timeout_secs: a SERVICE has no timeout.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(state.running.lock().await.contains_key(&target.id));
        assert!(start_run(&state, &target, "MANUAL").await.is_err());

        assert!(stop_run(&state, target.id).await);
        let rec = wait_done(&state, run_id).await;
        assert_eq!(rec.status, "SUCCESS");
        assert!(rec.captured_logs.unwrap().contains("stopped by operator"));
        assert!(!stop_run(&state, target.id).await);
    }

    #[tokio::test]
    async fn missing_script_is_rejected_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mut target) = setup(dir.path(), "nope.bat", "TASK", 5).await;
        assert!(start_run(&state, &target, "MANUAL").await.is_err());
        target.script_name = "..\\evil.bat".into();
        assert!(start_run(&state, &target, "MANUAL").await.is_err());
        assert!(state.db.list_runs(target.id, 10).await.unwrap().is_empty());
    }
}
