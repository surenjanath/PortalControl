//! Self-healing scanner tools.
//!
//! Security scans need `bandit` and `pip-audit`. Rather than asking the user
//! to install them, PortalControl keeps its own virtualenv in the data folder
//! (`tools/venv`) and installs them there: on demand when a scan finds a tool
//! missing or broken, or from Settings > Scanner Tools. Nothing is installed
//! into the system Python or into any project's environment.
//!
//! Lookup order for a tool (see `security::run_tool`): this managed
//! environment, then the program on PATH, then `python -m <module>`.

use crate::runner::hide_window;
use crate::state::AppState;
use crate::util::log;
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

/// Python packages that make up the toolbox, with the program each provides.
pub const TOOLS: [(&str, &str); 2] = [("bandit", "bandit"), ("pip-audit", "pip-audit")];
const VENV_TIMEOUT: Duration = Duration::from_secs(180);
const PIP_TIMEOUT: Duration = Duration::from_secs(900);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// What `ensure` should do.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Install what is missing.
    Install,
    /// Reinstall everything, for a copy that is present but broken.
    Repair,
    /// Move to the newest releases.
    Upgrade,
}

/// Outcome of the last install or repair, for the Settings page.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LastInstall {
    pub at: Option<String>,
    pub ok: Option<bool>,
    pub message: Option<String>,
}

pub fn venv_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join("venv")
}

fn bin_dir(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The program `name` inside the managed environment, if it is installed.
pub fn managed_program(state: &AppState, name: &str) -> Option<PathBuf> {
    let p = bin_dir(&venv_dir(state.data_dir.get()?)).join(exe(name));
    p.is_file().then_some(p)
}

fn venv_python(venv: &Path) -> PathBuf {
    bin_dir(venv).join(exe("python"))
}

struct Output {
    ok: bool,
    text: String,
}

async fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_window(&mut cmd);
    let child = cmd.spawn().map_err(|e| format!("{}: {e}", program.display()))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Output {
            ok: out.status.success(),
            text: format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        }),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// A Python on this machine that can create virtualenvs: `(program, extra
/// args, version)`. Windows tries the `py` launcher first because a bare
/// `python` there may be the Microsoft Store placeholder.
async fn system_python() -> Option<(PathBuf, Vec<&'static str>, String)> {
    let candidates: &[(&str, &[&str])] = if cfg!(windows) {
        &[("py", &["-3"]), ("python", &[]), ("python3", &[])]
    } else {
        &[("python3", &[]), ("python", &[])]
    };
    for (program, pre) in candidates {
        let mut args: Vec<&str> = pre.to_vec();
        args.extend(["-c", "import sys, venv; print('%d.%d.%d' % sys.version_info[:3])"]);
        if let Ok(out) = run(Path::new(program), &args, PROBE_TIMEOUT).await {
            let version = out.text.trim().to_string();
            if out.ok && version.starts_with('3') {
                return Some((PathBuf::from(program), pre.to_vec(), version));
            }
        }
    }
    None
}

/// First line of `<program> --version`, if it runs.
async fn version_of(program: &Path) -> Option<String> {
    let out = run(program, &["--version"], PROBE_TIMEOUT).await.ok()?;
    out.ok.then(|| out.text.lines().next().unwrap_or("").trim().to_string())
}

/// Where each tool would come from right now, plus Python and install state.
pub async fn status(state: &Arc<AppState>) -> serde_json::Value {
    let python = system_python().await;
    let mut tools = Vec::new();
    for (package, program) in TOOLS {
        let (source, version) = if let Some(p) = managed_program(state, program) {
            match version_of(&p).await {
                Some(v) => ("managed", Some(v)),
                None => ("broken", None),
            }
        } else if let Some(v) = version_of(Path::new(program)).await {
            ("system", Some(v))
        } else {
            ("missing", None)
        };
        tools.push(json!({"name": package, "source": source, "version": version}));
    }
    let installing = state.tools.try_lock().is_err();
    let last = if installing { LastInstall::default() } else { state.tools.lock().await.clone() };
    json!({
        "python": python.as_ref().map(|(p, pre, v)| json!({
            "command": format!("{} {}", p.display(), pre.join(" ")).trim().to_string(),
            "version": v,
        })),
        "dir": state.data_dir.get().map(|d| venv_dir(d).display().to_string()),
        "tools": tools,
        "installing": installing,
        "last": last,
    })
}

/// Create or repair the managed environment and install (or, with
/// `upgrade`, update) the tools. Waits if another install is in progress and
/// then returns its outcome rather than installing twice.
pub async fn ensure(state: &Arc<AppState>, mode: Mode, why: &str) -> Result<(), String> {
    let mut last = match state.tools.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            // Someone else is installing: wait for them and report their result.
            let guard = state.tools.lock().await;
            return match guard.ok {
                Some(true) => Ok(()),
                _ => {
                    Err(guard.message.clone().unwrap_or_else(|| "installing the scanner tools failed".into()))
                }
            };
        }
    };
    state.emit(json!({"type": "tools_changed", "installing": true}));
    state.activity("info", "tools", None, format!("Installing scanner tools ({why})"));
    let result = install(state, mode).await;
    *last = LastInstall {
        at: Some(crate::util::fmt_utc(crate::util::now_secs())),
        ok: Some(result.is_ok()),
        message: Some(match &result {
            Ok(v) => v.clone(),
            Err(e) => e.clone(),
        }),
    };
    match &result {
        Ok(v) => state.activity("ok", "tools", None, format!("Scanner tools ready: {v}")),
        Err(e) => state.activity("error", "tools", None, format!("Installing scanner tools failed: {e}")),
    }
    drop(last);
    state.emit(json!({"type": "tools_changed", "installing": false}));
    result.map(|_| ())
}

async fn install(state: &Arc<AppState>, mode: Mode) -> Result<String, String> {
    let data = state.data_dir.get().ok_or("data folder unknown")?;
    let venv = venv_dir(data);
    let mut py = venv_python(&venv);
    if !py.is_file() || run(&py, &["-c", "import pip"], PROBE_TIMEOUT).await.map(|o| !o.ok).unwrap_or(true) {
        // Missing or damaged (e.g. the Python it was made from was removed): start over.
        let (sys_py, pre, version) = system_python().await.ok_or_else(|| {
            if cfg!(windows) {
                "no Python 3 found. Install it from python.org (tick \"Add python.exe to PATH\"), then try again"
                    .to_string()
            } else {
                "no Python 3 with the venv module found. Install python3 (on Debian/Ubuntu also python3-venv), then try again"
                    .to_string()
            }
        })?;
        if venv.exists() {
            let _ = std::fs::remove_dir_all(&venv);
        }
        if let Some(parent) = venv.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        log(&format!("toolbox: creating {} with Python {version}", venv.display()));
        let venv_arg = venv.to_string_lossy().into_owned();
        let mut args: Vec<&str> = pre.clone();
        args.extend(["-m", "venv", venv_arg.as_str()]);
        let out = run(&sys_py, &args, VENV_TIMEOUT).await?;
        if !out.ok {
            return Err(format!("creating the environment failed: {}", tail(&out.text)));
        }
        py = venv_python(&venv);
    }
    // A copy that is present but does not run is not fixed by a plain
    // install ("already satisfied"), so that becomes a repair.
    let mut mode = mode;
    if mode == Mode::Install {
        for (_, program) in TOOLS {
            if let Some(p) = managed_program(state, program) {
                if version_of(&p).await.is_none() {
                    log(&format!("toolbox: {program} is present but does not run; reinstalling"));
                    mode = Mode::Repair;
                }
            }
        }
    }
    let mut args = vec!["-m", "pip", "install", "--disable-pip-version-check"];
    match mode {
        Mode::Install => {}
        Mode::Repair => args.push("--force-reinstall"),
        Mode::Upgrade => args.push("--upgrade"),
    }
    args.extend(TOOLS.iter().map(|(package, _)| *package));
    log(&format!("toolbox: pip {}", args[2..].join(" ")));
    let out = run(&py, &args, PIP_TIMEOUT).await?;
    if !out.ok {
        return Err(format!("pip install failed: {}", tail(&out.text)));
    }
    let mut versions = Vec::new();
    for (_, program) in TOOLS {
        let p = managed_program(state, program).ok_or_else(|| format!("{program} was not installed"))?;
        versions
            .push(version_of(&p).await.ok_or_else(|| format!("{program} was installed but does not run"))?);
    }
    Ok(versions.join(", "))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    lines[lines.len().saturating_sub(4)..].join(" | ")
}

/// Does this tool failure look like a missing or broken installation (which
/// installing again fixes), rather than a problem with the project?
pub fn looks_broken(stderr: &str) -> bool {
    ["No module named", "ModuleNotFoundError", "ImportError", "bad interpreter", "is not recognized"]
        .iter()
        .any(|m| stderr.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_platform_layout() {
        let v = venv_dir(Path::new("/data"));
        assert!(v.ends_with(Path::new("tools").join("venv")));
        let bandit = bin_dir(&v).join(exe("bandit"));
        if cfg!(windows) {
            assert!(bandit.ends_with(Path::new("Scripts").join("bandit.exe")));
        } else {
            assert!(bandit.ends_with(Path::new("bin").join("bandit")));
        }
    }

    #[test]
    fn broken_installs_are_recognised() {
        assert!(looks_broken("Traceback ...\nModuleNotFoundError: No module named 'bandit'"));
        assert!(looks_broken("/usr/bin/bandit: bad interpreter: No such file"));
        assert!(!looks_broken("[main] INFO profile include tests: None"));
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd\ne\n"), "b | c | d | e");
        assert_eq!(tail("only"), "only");
    }
}
