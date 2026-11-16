//! Project detection, generated start scripts, folder browsing, and companion
//! processes (Django-Q, Celery, npm scripts) with their launchers.

use super::*;

/// Every generated script carries this, so PortalControl can tell its own
/// launchers (safe to regenerate) from files the user wrote (never touched).
pub(super) const GENERATED_MARKER: &str = "Created by PortalControl";

/// A way to run a project that has no script of its own. PortalControl can
/// write the launcher script for it. `{host}` and `{port}` are filled in.
#[derive(Debug, Clone)]
pub(super) struct Starter {
    pub(super) id: &'static str,
    pub(super) label: String,
    pub(super) description: String,
    pub(super) lines: Vec<String>,
    pub(super) default_port: Option<u16>,
    pub(super) run_mode: &'static str,
    /// Pass the port as the `PORT` environment variable instead of an argument.
    pub(super) port_env: bool,
}

impl Starter {
    pub(super) fn script(&self) -> String {
        let ext = if cfg!(windows) { "bat" } else { "sh" };
        format!("portalcontrol_{}.{ext}", self.id.replace('-', "_"))
    }

    pub(super) fn body(&self, host: &str, port: Option<u16>) -> String {
        let port_text = port.map(|p| p.to_string()).unwrap_or_default();
        let lines: Vec<String> =
            self.lines.iter().map(|l| l.replace("{host}", host).replace("{port}", &port_text)).collect();
        let many = lines.len() > 1;
        let mut out = String::new();
        if cfg!(windows) {
            out.push_str(&format!(
                "@echo off\r\nREM {GENERATED_MARKER}: {}. Change it from the dashboard rather than by hand.\r\ncd /d \"%~dp0\"\r\n",
                self.label
            ));
            if self.port_env {
                out.push_str(&format!("set PORT={port_text}\r\nset HOST={host}\r\n"));
            }
            for line in &lines {
                // A multi-step script stops at the first step that fails.
                out.push_str(&if many { format!("{line} || exit /b 1\r\n") } else { format!("{line}\r\n") });
            }
        } else {
            out.push_str(&format!(
                "#!/bin/sh\n# {GENERATED_MARKER}: {}. Change it from the dashboard rather than by hand.\ncd \"$(dirname \"$0\")\" || exit 1\nset -e\n",
                self.label
            ));
            if self.port_env {
                out.push_str(&format!("export PORT={port_text}\nexport HOST={host}\n"));
            }
            for (i, line) in lines.iter().enumerate() {
                let last = i + 1 == lines.len();
                out.push_str(&if last && !many { format!("exec {line}\n") } else { format!("{line}\n") });
            }
        }
        out
    }
}

pub(super) struct ProjectInfo {
    pub(super) kind: &'static str,
    pub(super) label: &'static str,
    pub(super) starters: Vec<Starter>,
}

/// Lower-cased text of the Python dependency manifests at the top level.
pub(super) fn python_deps(dir: &std::path::Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(2_000) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if (name.starts_with("requirements") && name.ends_with(".txt"))
                || name == "pyproject.toml"
                || name == "pipfile"
            {
                text.push_str(&read_small(&entry.path()));
                text.push('\n');
            }
        }
    }
    text
}

/// First immediate sub-directory that contains `file`, e.g. the Django
/// project package (the one holding `wsgi.py`).
pub(super) fn package_with(dir: &std::path::Path, file: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .take(2_000)
        .filter(|e| e.path().join(file).is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Work out what kind of project a folder holds and how it could be started.
pub(super) fn detect_project(dir: &std::path::Path) -> ProjectInfo {
    let python = project_python(dir);
    let deps = python_deps(dir);
    let has = |f: &str| dir.join(f).is_file();
    let service = |id, label: &str, description: &str, line: String, port: u16| Starter {
        id,
        label: label.to_string(),
        description: description.to_string(),
        lines: vec![line],
        default_port: Some(port),
        run_mode: "SERVICE",
        port_env: false,
    };
    let mut starters = Vec::new();
    let (mut kind, mut label) = ("unknown", "Unrecognised project");

    if has("manage.py") {
        (kind, label) = ("django", "Django");
        starters.push(service(
            "runserver",
            "Django development server",
            "manage.py runserver. Simple and fine for internal tools; Django itself advises a WSGI server for production.",
            format!("{python} manage.py runserver {{host}}:{{port}} --noreload"),
            8000,
        ));
        if let Some(pkg) = package_with(dir, "wsgi.py") {
            if deps.contains("waitress") {
                starters.push(service(
                    "waitress",
                    "Waitress (production WSGI server)",
                    "Serves the project with Waitress, which is in its requirements. Suitable for production on Windows.",
                    format!("{python} -m waitress --listen={{host}}:{{port}} {pkg}.wsgi:application"),
                    8000,
                ));
            }
            if deps.contains("gunicorn") && !cfg!(windows) {
                starters.push(service(
                    "gunicorn",
                    "Gunicorn (production WSGI server)",
                    "Serves the project with Gunicorn, which is in its requirements.",
                    format!("{python} -m gunicorn --bind {{host}}:{{port}} {pkg}.wsgi:application"),
                    8000,
                ));
            }
        }
        let mut lines = Vec::new();
        if has("requirements.txt") {
            lines.push(format!("{python} -m pip install -r requirements.txt"));
        }
        lines.push(format!("{python} manage.py migrate --noinput"));
        let collects = package_with(dir, "settings.py")
            .map(|pkg| read_small(&dir.join(pkg).join("settings.py")).contains("static_root"))
            .unwrap_or(false);
        if collects {
            lines.push(format!("{python} manage.py collectstatic --noinput"));
        }
        starters.push(Starter {
            id: "deploy",
            label: "Django deploy task".into(),
            description: "Installs requirements, applies migrations and collects static files, stopping at the first step that fails. Runs to completion; pair it with a server target.".into(),
            lines,
            default_port: None,
            run_mode: "TASK",
            port_env: false,
        });
    } else if deps.contains("fastapi")
        && (has("main.py") || has("app.py") || dir.join("app").join("main.py").is_file())
    {
        (kind, label) = ("fastapi", "FastAPI");
        let module = if has("main.py") {
            "main"
        } else if has("app.py") {
            "app"
        } else {
            "app.main"
        };
        starters.push(service(
            "uvicorn",
            "Uvicorn server",
            "Serves the FastAPI app with Uvicorn.",
            format!("{python} -m uvicorn {module}:app --host {{host}} --port {{port}}"),
            8000,
        ));
    } else if deps.contains("streamlit") && (has("app.py") || has("main.py") || has("streamlit_app.py")) {
        (kind, label) = ("streamlit", "Streamlit");
        let file = if has("streamlit_app.py") {
            "streamlit_app.py"
        } else if has("app.py") {
            "app.py"
        } else {
            "main.py"
        };
        starters.push(service(
            "streamlit",
            "Streamlit app",
            "Runs the Streamlit app without opening a browser.",
            format!("{python} -m streamlit run {file} --server.port {{port}} --server.address {{host}} --server.headless true"),
            8501,
        ));
    } else if deps.contains("flask") && (has("app.py") || has("wsgi.py") || has("application.py")) {
        (kind, label) = ("flask", "Flask");
        let module = if has("app.py") {
            "app"
        } else if has("wsgi.py") {
            "wsgi"
        } else {
            "application"
        };
        starters.push(service(
            "flask",
            "Flask server",
            "flask run. Fine for internal tools; use a WSGI server for production.",
            format!("{python} -m flask --app {module} run --host {{host}} --port {{port}}"),
            5000,
        ));
    }

    let scripts = package_scripts(dir);
    if has("package.json") {
        if kind == "unknown" {
            (kind, label) = ("node", "Node.js");
        }
        for (script, id, text) in [("start", "npm-start", "npm start"), ("dev", "npm-dev", "npm dev server")]
        {
            if scripts.iter().any(|s| s == script) {
                starters.push(Starter {
                    id,
                    label: text.to_string(),
                    description: format!("Runs the \"{script}\" script from package.json. The port is passed as the PORT environment variable, which most Node servers honour; if this one does not, the port set here only affects the health check."),
                    lines: vec![format!("npm run {script}")],
                    default_port: Some(3000),
                    run_mode: "SERVICE",
                    port_env: true,
                });
            }
        }
    }
    if kind == "unknown" && has("index.html") {
        (kind, label) = ("static", "Static site");
        starters.push(service(
            "static",
            "Static file server",
            "Serves this folder with Python's built-in web server.",
            format!(
                "{} -m http.server {{port}} --bind {{host}}",
                pyenv::system_python(std::env::var_os("PATH"))
            ),
            8080,
        ));
    }
    if kind == "unknown" && !deps.is_empty() {
        (kind, label) = ("python", "Python");
    }
    ProjectInfo { kind, label, starters }
}

/// Write (or regenerate) the launcher script of a starter. A file that was
/// not generated by PortalControl is never overwritten.
pub(super) fn create_starter(
    dir: &std::path::Path,
    id: &str,
    host: &str,
    port: Option<u16>,
) -> Result<(String, Option<String>), String> {
    if !dir.is_absolute() || !dir.is_dir() {
        return Err("directory does not exist".into());
    }
    if host != "127.0.0.1" && host != "0.0.0.0" {
        return Err("host must be 127.0.0.1 or 0.0.0.0".into());
    }
    let starter = detect_project(dir)
        .starters
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("'{id}' is not available for this project"))?;
    let port = match (starter.default_port, port) {
        (None, _) => None,
        (Some(_), Some(p)) if p >= 1 => Some(p),
        (Some(_), _) => return Err("a port between 1 and 65535 is required".into()),
    };
    let script = starter.script();
    let path = dir.join(&script);
    if path.exists() && !std::fs::read_to_string(&path).unwrap_or_default().contains(GENERATED_MARKER) {
        return Err(format!(
            "{script} already exists and was not created by PortalControl. Choose it as the script instead."
        ));
    }
    std::fs::write(&path, starter.body(host, port))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((script, port.map(|p| format!("http://127.0.0.1:{p}/"))))
}

#[derive(serde::Deserialize)]
pub(super) struct StarterRequest {
    pub(super) dir: String,
    pub(super) starter: String,
    #[serde(default)]
    pub(super) host: Option<String>,
    #[serde(default)]
    pub(super) port: Option<u16>,
}

pub(super) async fn fs_starter(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StarterRequest>,
) -> AppResult {
    let dir = std::path::PathBuf::from(req.dir.trim().trim_matches('"'));
    let shown = dir.display().to_string();
    let (script, health_url) = tokio::task::spawn_blocking(move || {
        create_starter(&dir, &req.starter, req.host.as_deref().unwrap_or("127.0.0.1"), req.port)
    })
    .await
    .map_err(|e| internal(e.to_string()))?
    .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity("info", "system", None, format!("Start script {script} written in {shown}"));
    Ok(ok(json!({"script": script, "health_url": health_url})))
}

/// Is something already listening on this port on this machine?
pub(super) fn port_in_use(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250)).is_ok()
        || std::net::TcpListener::bind(addr).is_err()
}

/// Check a port before it goes into a start script: whether something is
/// listening on it now, whether another target already claims it, and the
/// next port that is clear of both.
/// Which Python environment a setting leads to, before it is saved.
pub(super) async fn fs_pyenv(Query(q): Params) -> AppResult {
    let dir = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    if dir.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "dir is required"));
    }
    let setting = q.get("env").cloned();
    let shown =
        tokio::task::spawn_blocking(move || pyenv::describe(std::path::Path::new(&dir), setting.as_deref()))
            .await
            .map_err(|e| internal(e.to_string()))?;
    Ok(ok(shown))
}

pub(super) async fn fs_port(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let Some(port) = q.get("port").and_then(|p| p.parse::<u16>().ok()).filter(|p| *p > 0) else {
        return Err(fail(StatusCode::BAD_REQUEST, "port must be a number between 1 and 65535"));
    };
    let except: Option<i64> = q.get("except").and_then(|v| v.parse().ok());
    let claimed: Vec<(u16, String)> = state
        .db
        .list_targets()
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|t| Some(t.id) != except)
        .filter_map(|t| {
            let url = t.health_url.as_deref()?;
            health::parse_url(url).ok().map(|u| (u.port, t.name.clone()))
        })
        .collect();
    let report = tokio::task::spawn_blocking(move || {
        let used_by = claimed.iter().find(|(p, _)| *p == port).map(|(_, n)| n.clone());
        let in_use = port_in_use(port);
        let suggestion = if in_use || used_by.is_some() {
            (port..=port.saturating_add(200))
                .find(|p| !claimed.iter().any(|(c, _)| c == p) && !port_in_use(*p))
        } else {
            None
        };
        json!({"port": port, "in_use": in_use, "used_by": used_by, "suggestion": suggestion})
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    Ok(ok(report))
}

/// Sub-folders of a directory, for the folder picker on the New target page.
pub(super) async fn fs_list(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let wanted = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    // The drives the metrics worker already knows about. Probing every drive
    // letter here would stall for seconds on disconnected network drives.
    let mut roots: Vec<String> =
        state.metrics.lock().map(|m| m.disks.iter().map(|d| d.mount.clone()).collect()).unwrap_or_default();
    if roots.is_empty() {
        roots.push(if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() });
    }
    let report = tokio::task::spawn_blocking(move || list_dir(&wanted, roots))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(report))
}

pub(super) fn list_dir(wanted: &str, roots: Vec<String>) -> Value {
    let home =
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from);
    let dir = if wanted.is_empty() {
        home.clone().unwrap_or_else(|| std::path::PathBuf::from(roots.first().cloned().unwrap_or_default()))
    } else {
        std::path::PathBuf::from(wanted)
    };
    if !dir.is_absolute() || !dir.is_dir() {
        return json!({"error": "that folder does not exist", "roots": roots, "home": home});
    }
    let markers = [".git", "manage.py", "package.json", "requirements.txt", "pyproject.toml"];
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(&dir)
        .map(|list| {
            list.flatten()
                .take(5_000)
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .filter(|(name, _)| !name.starts_with('.') && !name.starts_with('$'))
                .map(|(name, path)| {
                    let project = markers.iter().any(|m| path.join(m).exists());
                    (name, project)
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|a| a.0.to_lowercase());
    entries.truncate(600);
    json!({
        "dir": dir.display().to_string(),
        "parent": dir.parent().map(|p| p.display().to_string()),
        "is_project": markers.iter().any(|m| dir.join(m).exists()),
        "entries": entries.iter().map(|(name, project)| json!({
            "name": name,
            "path": dir.join(name).display().to_string(),
            "project": project,
        })).collect::<Vec<_>>(),
        "roots": roots,
        "home": home,
    })
}

// ---------------------------------------------------- companion processes --

/// A background process a project needs besides its main script, e.g. a
/// Django-Q cluster or a Celery worker.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Worker {
    pub(super) kind: &'static str,
    pub(super) label: &'static str,
    /// The command the launcher runs, relative to the project directory.
    pub(super) command: String,
    /// File name of the launcher script PortalControl can create.
    pub(super) script: String,
    /// What gave it away, shown to the operator.
    pub(super) why: String,
    /// Scripts in the project folder that already start this process. Adding
    /// it as a target next to one of those runs it twice.
    pub(super) started_by: Vec<String>,
}

/// Words that, together on one line of a script, mean it starts this worker.
fn start_words(kind: &str) -> &'static [&'static str] {
    match kind {
        "qcluster" => &["manage.py", "qcluster"],
        "celery" => &["celery", " worker"],
        "celery_beat" => &["celery", " beat"],
        "npm_start" => &["npm", " start"],
        "npm_dev" => &["npm", "run dev"],
        "npm_worker" => &["npm", "run worker"],
        _ => &[],
    }
}

/// The scripts of this platform at the top of `dir` that start the worker
/// themselves (comment lines do not count), leaving out its own launcher.
pub(super) fn scripts_starting(dir: &std::path::Path, kind: &str, own_script: &str) -> Vec<String> {
    let words = start_words(kind);
    if words.is_empty() {
        return Vec::new();
    }
    let exts: &[&str] = if cfg!(windows) { &["bat", "cmd", "ps1"] } else { &["sh", "command"] };
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .take(2_000)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let ext = name.rsplit_once('.').map(|(_, x)| x.to_lowercase()).unwrap_or_default();
                    if !exts.contains(&ext.as_str()) || name.eq_ignore_ascii_case(own_script) {
                        return None;
                    }
                    let starts = read_small(&e.path()).lines().map(str::trim).any(|l| {
                        let comment = l.starts_with('#') || l.starts_with("rem ") || l.starts_with("::");
                        !comment && words.iter().all(|w| l.contains(w))
                    });
                    starts.then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

pub(super) fn read_small(path: &std::path::Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() <= 512 * 1024 => {
            std::fs::read_to_string(path).unwrap_or_default().to_lowercase()
        }
        _ => String::new(),
    }
}

/// The Python interpreter a launcher should use: the project's own
/// virtualenv when it has one for this platform, else the system Python.
pub(super) fn project_python(dir: &std::path::Path) -> String {
    for venv in [".venv", "venv", "env"] {
        let (probe, shown) = if cfg!(windows) {
            (dir.join(venv).join("Scripts").join("python.exe"), format!("{venv}\\Scripts\\python.exe"))
        } else {
            (dir.join(venv).join("bin").join("python"), format!("{venv}/bin/python"))
        };
        if probe.is_file() {
            return shown;
        }
    }
    pyenv::system_python(std::env::var_os("PATH")).to_string()
}

/// Long-running npm scripts that are worth supervising as a process.
pub(super) const NPM_PROCESSES: &[(&str, &str, &str)] = &[
    ("start", "npm_start", "npm start"),
    ("dev", "npm_dev", "npm dev server"),
    ("worker", "npm_worker", "npm worker"),
];

/// Every companion process PortalControl recognises in a project folder.
pub(super) fn detect_workers(dir: &std::path::Path) -> Vec<Worker> {
    let mut out = detect_django_workers(dir);
    let ext = if cfg!(windows) { "bat" } else { "sh" };
    let defined = package_scripts(dir);
    for (script, kind, label) in NPM_PROCESSES {
        if defined.iter().any(|s| s == script) {
            out.push(Worker {
                kind,
                label,
                command: format!("npm run {script}"),
                script: format!("portalcontrol_{kind}.{ext}"),
                why: format!("\"{script}\" script in package.json"),
                started_by: Vec::new(),
            });
        }
    }
    for w in &mut out {
        w.started_by = scripts_starting(dir, w.kind, &w.script);
    }
    out
}

/// Look for task-queue workers that belong to a Django project: Django-Q
/// (`manage.py qcluster`) and Celery (worker, and beat when it is configured).
pub(super) fn detect_django_workers(dir: &std::path::Path) -> Vec<Worker> {
    if !dir.join("manage.py").is_file() {
        return Vec::new();
    }
    // Dependency manifests at the top level.
    let mut deps: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(2_000) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let is_manifest = (name.starts_with("requirements") && name.ends_with(".txt"))
                || name == "pyproject.toml"
                || name == "pipfile";
            if is_manifest {
                deps.push((entry.file_name().to_string_lossy().into_owned(), read_small(&entry.path())));
            }
        }
    }
    // Settings modules one level down: <pkg>/settings.py or <pkg>/settings/*.py.
    let mut settings: Vec<(String, String)> = Vec::new();
    let mut celery_app: Option<String> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(2_000) {
            let pkg = entry.path();
            let pkg_name = entry.file_name().to_string_lossy().into_owned();
            if !pkg.is_dir() || pkg_name.starts_with('.') || pkg_name == "node_modules" {
                continue;
            }
            if pkg.join("celery.py").is_file() && celery_app.is_none() {
                celery_app = Some(pkg_name.clone());
            }
            let single = pkg.join("settings.py");
            if single.is_file() {
                settings.push((format!("{pkg_name}/settings.py"), read_small(&single)));
            }
            if let Ok(files) = std::fs::read_dir(pkg.join("settings")) {
                for f in files.flatten().take(50) {
                    let fname = f.file_name().to_string_lossy().into_owned();
                    if fname.ends_with(".py") {
                        settings.push((format!("{pkg_name}/settings/{fname}"), read_small(&f.path())));
                    }
                }
            }
        }
    }
    let find = |sources: &[(String, String)], needles: &[&str]| -> Option<String> {
        sources
            .iter()
            .find(|(_, text)| needles.iter().any(|n| text.contains(n)))
            .map(|(name, _)| name.clone())
    };

    let python = project_python(dir);
    let ext = if cfg!(windows) { "bat" } else { "sh" };
    let mut out = Vec::new();

    let q = find(&settings, &["django_q", "q_cluster"]).or_else(|| find(&deps, &["django-q", "django_q"]));
    if let Some(source) = q {
        out.push(Worker {
            kind: "qcluster",
            label: "Django-Q cluster",
            command: format!("{python} manage.py qcluster"),
            script: format!("portalcontrol_qcluster.{ext}"),
            why: format!("django_q found in {source}"),
            started_by: Vec::new(),
        });
    }

    let celery_dep = find(&deps, &["celery"]);
    if let Some(app) = celery_app.filter(|_| celery_dep.is_some() || find(&settings, &["celery"]).is_some()) {
        // The default prefork pool does not work on Windows.
        let pool = if cfg!(windows) { " --pool=solo" } else { "" };
        out.push(Worker {
            kind: "celery",
            label: "Celery worker",
            command: format!("{python} -m celery -A {app} worker --loglevel=info{pool}"),
            script: format!("portalcontrol_celery.{ext}"),
            why: format!("{app}/celery.py"),
            started_by: Vec::new(),
        });
        if let Some(source) = find(&settings, &["django_celery_beat", "beat_schedule"]) {
            out.push(Worker {
                kind: "celery_beat",
                label: "Celery beat",
                command: format!("{python} -m celery -A {app} beat --loglevel=info"),
                script: format!("portalcontrol_celery_beat.{ext}"),
                why: format!("beat schedule found in {source}"),
                started_by: Vec::new(),
            });
        }
    }
    out
}

pub(super) fn launcher_body(worker: &Worker) -> String {
    if cfg!(windows) {
        format!(
            "@echo off\r\nREM Created by PortalControl: starts the {} for this project.\r\n\
             cd /d \"%~dp0\"\r\n{}\r\n",
            worker.label, worker.command
        )
    } else {
        format!(
            "#!/bin/sh\n# Created by PortalControl: starts the {} for this project.\n\
             cd \"$(dirname \"$0\")\" || exit 1\nexec {}\n",
            worker.label, worker.command
        )
    }
}

/// Write the launcher script for a detected worker. Only the fixed, generated
/// content for a worker that was actually detected is ever written, and an
/// existing file is never overwritten.
pub(super) fn create_launcher(dir: &std::path::Path, kind: &str) -> Result<(String, bool), String> {
    if !dir.is_absolute() || !dir.is_dir() {
        return Err("directory does not exist".into());
    }
    let worker = detect_workers(dir)
        .into_iter()
        .find(|w| w.kind == kind)
        .ok_or_else(|| format!("no '{kind}' worker was detected in this directory"))?;
    let path = dir.join(&worker.script);
    if path.exists() {
        return Ok((worker.script, false));
    }
    std::fs::write(&path, launcher_body(&worker))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((worker.script, true))
}

#[derive(serde::Deserialize)]
pub(super) struct LauncherRequest {
    pub(super) dir: String,
    pub(super) kind: String,
}

pub(super) async fn fs_launcher(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LauncherRequest>,
) -> AppResult {
    let dir = std::path::PathBuf::from(req.dir.trim().trim_matches('"'));
    let shown = dir.display().to_string();
    let kind = req.kind;
    let (script, created) = tokio::task::spawn_blocking(move || create_launcher(&dir, &kind))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    if created {
        state.activity("info", "system", None, format!("Launcher script {script} created in {shown}"));
    }
    Ok(ok(json!({"script": script, "created": created})))
}
