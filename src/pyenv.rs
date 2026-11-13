//! The Python environment (virtualenv or conda environment) of a target.
//!
//! A target's `python_env` setting is one of:
//!   * empty: nothing is activated; scripts run with whatever is on PATH
//!   * `auto`: use `.venv`, `venv` or `env` in the working directory, if any
//!   * a folder: use that environment, wherever it lives (a relative path is
//!     taken from the working directory)
//!
//! "Activating" means what an `activate` script does: the environment's
//! program folder goes to the front of PATH and `VIRTUAL_ENV` is set, so a
//! plain `python` or `pip` inside the target's script is the environment's.

use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const AUTO: &str = "auto";
/// Folder names tried by `auto`, in this order.
const NAMES: [&str; 3] = [".venv", "venv", "env"];

#[derive(Debug, Clone, PartialEq)]
pub struct PyEnv {
    pub root: PathBuf,
    pub python: PathBuf,
}

/// The interpreter of the environment at `root`, if there is one for this
/// platform. A virtualenv made on Linux has no `Scripts\python.exe`, so it
/// is not an environment on Windows, and the other way round.
fn python_in(root: &Path) -> Option<PathBuf> {
    let candidates: &[&[&str]] = if cfg!(windows) {
        // A virtualenv, then a conda environment.
        &[&["Scripts", "python.exe"], &["python.exe"]]
    } else {
        &[&["bin", "python"], &["bin", "python3"]]
    };
    candidates
        .iter()
        .map(|parts| parts.iter().fold(root.to_path_buf(), |p, part| p.join(part)))
        .find(|p| p.is_file())
}

/// The environment inside the working directory, if there is one.
pub fn detect(dir: &Path) -> Option<PyEnv> {
    NAMES.iter().find_map(|name| {
        let root = dir.join(name);
        python_in(&root).map(|python| PyEnv { root, python })
    })
}

/// The environment a target's setting stands for. `Ok(None)` means none is
/// used; an error means a folder was named that holds no Python.
pub fn resolve(dir: &Path, setting: Option<&str>) -> Result<Option<PyEnv>, String> {
    match setting.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(AUTO) => Ok(detect(dir)),
        Some(folder) => {
            // `join` keeps an absolute path as it is.
            let root = dir.join(folder);
            match python_in(&root) {
                Some(python) => Ok(Some(PyEnv { root, python })),
                None if !root.is_dir() => Err(format!("the folder does not exist: {}", root.display())),
                None => Err(format!(
                    "no Python found in {} (expected {})",
                    root.display(),
                    if cfg!(windows) { "Scripts\\python.exe or python.exe" } else { "bin/python" }
                )),
            }
        }
    }
}

/// The Python that the built-in commands run: the target's environment, else
/// an environment in the folder, else the system Python from PATH.
pub fn command_python(dir: &Path, setting: Option<&str>) -> String {
    resolve(dir, setting)
        .ok()
        .flatten()
        .or_else(|| detect(dir))
        .map(|e| e.python.to_string_lossy().into_owned())
        .unwrap_or_else(|| system_python(std::env::var_os("PATH")).to_string())
}

/// The name of the Python on PATH. Windows installs `python`; macOS and many
/// Linux systems ship only `python3`, so prefer that one there and fall back
/// to `python` when only that exists (or neither is found).
pub fn system_python(path: Option<OsString>) -> &'static str {
    if cfg!(windows) {
        return "python";
    }
    let dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let found = |name: &str| dirs.iter().any(|d| d.join(name).is_file());
    if !found("python3") && found("python") {
        "python"
    } else {
        "python3"
    }
}

impl PyEnv {
    /// Folders that go to the front of PATH.
    fn path_dirs(&self) -> Vec<PathBuf> {
        let dirs = if cfg!(windows) {
            // `Library\bin` holds the DLLs of a conda environment.
            vec![self.root.join("Scripts"), self.root.clone(), self.root.join("Library").join("bin")]
        } else {
            vec![self.root.join("bin")]
        };
        dirs.into_iter().filter(|d| d.is_dir()).collect()
    }

    fn search_path(&self) -> Option<OsString> {
        let current = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(self.path_dirs().into_iter().chain(std::env::split_paths(&current))).ok()
    }

    /// Activate this environment for `cmd`.
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        cmd.env("VIRTUAL_ENV", &self.root);
        if let Some(path) = self.search_path() {
            cmd.env("PATH", path);
        }
        // A leftover PYTHONHOME would point the interpreter at another install.
        cmd.env_remove("PYTHONHOME");
    }

    /// Python version recorded in the environment's `pyvenv.cfg`, if any.
    fn version(&self) -> Option<String> {
        let cfg = std::fs::read_to_string(self.root.join("pyvenv.cfg")).ok()?;
        cfg.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            matches!(key.trim(), "version" | "version_info").then(|| value.trim().to_string())
        })
    }
}

/// What the dashboard shows about a setting: which mode it is, and the
/// environment it leads to or why it leads nowhere.
pub fn describe(dir: &Path, setting: Option<&str>) -> Value {
    let setting = setting.map(str::trim).filter(|s| !s.is_empty());
    let mode = match setting {
        None => "off",
        Some(AUTO) => "auto",
        Some(_) => "custom",
    };
    match resolve(dir, setting) {
        Ok(Some(env)) => json!({
            "mode": mode, "found": true,
            "root": env.root.display().to_string(),
            "python": env.python.display().to_string(),
            "version": env.version(),
        }),
        Ok(None) => json!({"mode": mode, "found": false}),
        Err(e) => json!({"mode": mode, "found": false, "error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an empty environment the way this platform lays it out.
    fn make_env(root: &Path) -> PathBuf {
        let (folder, exe) = if cfg!(windows) { ("Scripts", "python.exe") } else { ("bin", "python") };
        std::fs::create_dir_all(root.join(folder)).unwrap();
        let python = root.join(folder).join(exe);
        std::fs::write(&python, "").unwrap();
        python
    }

    #[test]
    #[cfg(not(windows))]
    fn system_python_prefers_python3() {
        let tmp = tempfile::tempdir().unwrap();
        let (both, only_py, none) =
            (tmp.path().join("both"), tmp.path().join("only"), tmp.path().join("none"));
        for d in [&both, &only_py, &none] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(both.join("python3"), "").unwrap();
        std::fs::write(both.join("python"), "").unwrap();
        std::fs::write(only_py.join("python"), "").unwrap();
        assert_eq!(system_python(Some(both.into_os_string())), "python3");
        assert_eq!(system_python(Some(only_py.into_os_string())), "python");
        assert_eq!(system_python(Some(none.into_os_string())), "python3");
        assert_eq!(system_python(None), "python3");
    }

    #[test]
    fn off_auto_and_a_named_folder() {
        let project = tempfile::tempdir().unwrap();
        let dir = project.path();
        assert_eq!(resolve(dir, None), Ok(None));
        assert_eq!(resolve(dir, Some("  ")), Ok(None));
        assert_eq!(resolve(dir, Some(AUTO)), Ok(None), "auto with no environment is not an error");
        let system = system_python(std::env::var_os("PATH"));
        assert_eq!(command_python(dir, Some(AUTO)), system, "no environment: the system Python");

        // `venv` is found, and `.venv` wins once it exists.
        make_env(&dir.join("venv"));
        assert_eq!(resolve(dir, Some(AUTO)).unwrap().unwrap().root, dir.join("venv"));
        let python = make_env(&dir.join(".venv"));
        let env = resolve(dir, Some(AUTO)).unwrap().unwrap();
        assert_eq!((env.root.clone(), env.python.clone()), (dir.join(".venv"), python.clone()));

        // Off does not activate, but built-in commands still prefer the folder's environment.
        assert_eq!(command_python(dir, None), python.to_string_lossy());

        // A relative folder is taken from the working directory.
        assert_eq!(resolve(dir, Some("venv")).unwrap().unwrap().root, dir.join("venv"));
    }

    #[test]
    fn an_environment_outside_the_project() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let python = make_env(elsewhere.path());
        std::fs::write(elsewhere.path().join("pyvenv.cfg"), "home = /usr/bin\nversion = 3.12.4\n").unwrap();

        let setting = elsewhere.path().to_string_lossy().into_owned();
        let env = resolve(project.path(), Some(&setting)).unwrap().unwrap();
        assert_eq!(env.python, python);
        assert_eq!(command_python(project.path(), Some(&setting)), python.to_string_lossy());

        let shown = describe(project.path(), Some(&setting));
        assert_eq!(shown["mode"], "custom");
        assert_eq!(shown["found"], true);
        assert_eq!(shown["version"], "3.12.4");

        // Its program folder leads PATH.
        let first = std::env::split_paths(&env.search_path().unwrap()).next().unwrap();
        assert_eq!(first, python.parent().unwrap());
    }

    #[test]
    fn a_folder_without_python_is_an_error() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("empty")).unwrap();
        assert!(resolve(project.path(), Some("empty")).unwrap_err().contains("no Python found"));
        assert!(resolve(project.path(), Some("missing")).unwrap_err().contains("does not exist"));
        let shown = describe(project.path(), Some("missing"));
        assert_eq!(shown["found"], false);
        assert!(shown["error"].is_string());
        assert_eq!(describe(project.path(), None)["mode"], "off");
    }
}
