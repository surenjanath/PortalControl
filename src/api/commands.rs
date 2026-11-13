//! Quick commands: the fixed list of maintenance commands offered per project type.

use super::*;

/// A maintenance command PortalControl can run for a target. The program and
/// its arguments are fixed here; nothing typed in the dashboard is executed.
pub(super) struct QuickCommand {
    pub(super) id: &'static str,
    pub(super) label: &'static str,
    pub(super) description: &'static str,
    pub(super) args: &'static [&'static str],
    /// True when the command changes the database, files or environment.
    pub(super) changes: bool,
    /// Take a database backup first when the target has backups enabled.
    pub(super) backup_first: bool,
}

pub(super) const DJANGO_COMMANDS: &[QuickCommand] = &[
    QuickCommand {
        id: "check",
        label: "System check",
        description: "Validate the project configuration.",
        args: &["manage.py", "check"],
        changes: false,
        backup_first: false,
    },
    QuickCommand {
        id: "showmigrations",
        label: "Show migrations",
        description: "List every migration and whether it has been applied.",
        args: &["manage.py", "showmigrations"],
        changes: false,
        backup_first: false,
    },
    QuickCommand {
        id: "pending",
        label: "Check for model changes",
        description: "Report model changes that have no migration yet. Nothing is written.",
        args: &["manage.py", "makemigrations", "--check", "--dry-run"],
        changes: false,
        backup_first: false,
    },
    QuickCommand {
        id: "migrate",
        label: "Apply migrations",
        description: "Run pending database migrations.",
        args: &["manage.py", "migrate", "--noinput"],
        changes: true,
        backup_first: true,
    },
    QuickCommand {
        id: "collectstatic",
        label: "Collect static files",
        description: "Copy static files into STATIC_ROOT.",
        args: &["manage.py", "collectstatic", "--noinput"],
        changes: true,
        backup_first: false,
    },
    QuickCommand {
        id: "clearsessions",
        label: "Clear expired sessions",
        description: "Delete expired rows from the session store.",
        args: &["manage.py", "clearsessions"],
        changes: true,
        backup_first: false,
    },
];

pub(super) const PYTHON_COMMANDS: &[QuickCommand] = &[
    QuickCommand { id: "pip-install", label: "Install requirements", description: "pip install -r requirements.txt, into the project's virtualenv if it has one, otherwise into the Python on PATH.", args: &["-m", "pip", "install", "-r", "requirements.txt"], changes: true, backup_first: false },
    QuickCommand { id: "pip-outdated", label: "List outdated packages", description: "Show installed packages that have newer releases.", args: &["-m", "pip", "list", "--outdated"], changes: false, backup_first: false },
];

/// A quick command resolved for one project: which program runs it and with
/// which arguments.
#[derive(Debug, Clone)]
pub(super) struct ResolvedCommand {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) description: String,
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) changes: bool,
    pub(super) backup_first: bool,
}

impl ResolvedCommand {
    pub(super) fn shown(&self) -> String {
        let program = self.program.strip_suffix(".cmd").unwrap_or(&self.program);
        format!("{program} {}", self.args.join(" "))
    }
}

pub(super) fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

/// Names of the scripts defined in `package.json`, limited to plain names.
pub(super) fn package_scripts(dir: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { return Vec::new() };
    if text.len() > 1024 * 1024 {
        return Vec::new();
    }
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    parsed
        .get("scripts")
        .and_then(Value::as_object)
        .map(|scripts| {
            scripts
                .keys()
                .filter(|k| {
                    !k.is_empty()
                        && k.len() <= 40
                        && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// npm scripts offered as quick commands when the project defines them.
pub(super) const NPM_SCRIPTS: &[(&str, &str, &str, bool)] = &[
    ("build", "Build", "Run the project's build script.", true),
    ("lint", "Lint", "Run the project's lint script.", false),
    ("test", "Test", "Run the project's test script.", false),
    ("typecheck", "Type check", "Run the project's typecheck script.", false),
    ("migrate", "Run migrations", "Run the project's migrate script.", true),
];

pub(super) fn detect_commands(dir: &std::path::Path, python_env: Option<&str>) -> Vec<ResolvedCommand> {
    let mut out = Vec::new();
    let python = pyenv::command_python(dir, python_env);
    let mut from_python = |list: &[QuickCommand]| {
        for c in list {
            out.push(ResolvedCommand {
                id: c.id.to_string(),
                label: c.label.to_string(),
                description: c.description.to_string(),
                program: python.clone(),
                args: c.args.iter().map(|a| a.to_string()).collect(),
                changes: c.changes,
                backup_first: c.backup_first,
            });
        }
    };
    if dir.join("manage.py").is_file() {
        from_python(DJANGO_COMMANDS);
    }
    if dir.join("requirements.txt").is_file() {
        from_python(PYTHON_COMMANDS);
    }

    if dir.join("package.json").is_file() {
        let npm = |id: &str, label: &str, description: &str, args: &[&str], changes: bool| ResolvedCommand {
            id: id.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            program: npm_program().to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            changes,
            backup_first: false,
        };
        out.push(npm(
            "npm-install",
            "Install packages",
            "npm install: install the dependencies listed in package.json.",
            &["install"],
            true,
        ));
        if dir.join("package-lock.json").is_file() {
            out.push(npm(
                "npm-ci",
                "Clean install",
                "npm ci: delete node_modules and install exactly what package-lock.json lists.",
                &["ci"],
                true,
            ));
        }
        let defined = package_scripts(dir);
        for (script, label, description, changes) in NPM_SCRIPTS {
            if defined.iter().any(|s| s == script) {
                out.push(npm(&format!("npm-run-{script}"), label, description, &["run", script], *changes));
            }
        }
    }
    out
}

pub(super) async fn list_commands(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = std::path::PathBuf::from(&t.working_directory);
    let python = pyenv::command_python(&dir, t.python_env.as_deref());
    let commands: Vec<Value> = detect_commands(&dir, t.python_env.as_deref())
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "description": c.description,
                "command": c.shown(),
                "changes": c.changes,
                "backup_first": c.backup_first && t.backup_on_deploy,
            })
        })
        .collect();
    Ok(ok(json!({"python": python, "commands": commands})))
}

pub(super) async fn run_command(
    State(state): State<Arc<AppState>>,
    Path((id, command)): Path<(i64, String)>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = std::path::PathBuf::from(&t.working_directory);
    let Some(cmd) = detect_commands(&dir, t.python_env.as_deref()).into_iter().find(|c| c.id == command)
    else {
        return Err(fail(StatusCode::NOT_FOUND, "this command is not available for the target"));
    };
    if cmd.backup_first && t.backup_on_deploy {
        backup::before_run(&state, &t).await;
    }
    run_response(runner::start_command(&state, &t, &cmd.label, &cmd.program, &cmd.args).await)
}

pub(super) async fn stop_command(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    if runner::stop_run(&state, runner::command_slot(id)).await {
        Ok(ok(json!({"stopping": true})))
    } else {
        Err(fail(StatusCode::CONFLICT, "no command is running"))
    }
}
