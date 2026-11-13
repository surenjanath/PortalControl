//! Activity feed, cron preview and folder inspection for the New-target page.

use super::*;

pub(super) async fn activity(State(state): State<Arc<AppState>>) -> AppResult {
    let events: Vec<Value> =
        state.activity.lock().map(|ring| ring.iter().rev().cloned().collect()).unwrap_or_default();
    Ok(ok(json!(events)))
}

pub(super) async fn cron_preview(Query(q): Params) -> AppResult {
    let expr = q.get("expr").map(String::as_str).unwrap_or("");
    match automation::preview(expr, 5) {
        Ok(next) => Ok(ok(json!({"valid": true, "next": next}))),
        Err(e) => Ok(ok(json!({"valid": false, "error": e}))),
    }
}

/// Look at a directory before registering it: which scripts it holds, whether
/// it is a Git repository and what kind of project it appears to be.
pub(super) async fn fs_inspect(Query(q): Params) -> AppResult {
    let raw = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    let dir = std::path::PathBuf::from(&raw);
    if raw.is_empty() || !dir.is_absolute() {
        return Ok(ok(json!({"exists": false, "error": "enter an absolute path"})));
    }
    let report =
        tokio::task::spawn_blocking(move || inspect_dir(&dir)).await.map_err(|e| internal(e.to_string()))?;
    Ok(ok(report))
}

pub(super) fn inspect_dir(dir: &std::path::Path) -> Value {
    if !dir.is_dir() {
        return json!({"exists": false, "error": "directory does not exist"});
    }
    let extensions: &[&str] = if cfg!(windows) { &["bat", "cmd"] } else { &["sh"] };
    let is_script = |p: &std::path::Path| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .map(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false)
    };
    let mut scripts: Vec<String> = Vec::new();
    for (prefix, folder) in [("", dir.to_path_buf()), ("scripts/", dir.join("scripts"))] {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten().take(2_000) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let full = format!("{prefix}{name}");
            if is_script(&entry.path()) && runner::validate_script_name(&full).is_ok() {
                scripts.push(full);
            }
        }
    }
    scripts.sort();
    scripts.truncate(50);

    let markers = [
        ("manage.py", "Django"),
        ("requirements.txt", "Python (requirements.txt)"),
        ("pyproject.toml", "Python (pyproject.toml)"),
        ("package.json", "Node.js"),
        ("Cargo.toml", "Rust"),
        ("go.mod", "Go"),
        ("composer.json", "PHP"),
        ("docker-compose.yml", "Docker Compose"),
        ("Dockerfile", "Dockerfile"),
        (".env", ".env file"),
    ];
    let detected: Vec<&str> =
        markers.iter().filter(|(file, _)| dir.join(file).exists()).map(|(_, label)| *label).collect();

    let workers: Vec<Value> = detect_workers(dir)
        .iter()
        .map(|w| {
            json!({
                "kind": w.kind,
                "label": w.label,
                "command": w.command,
                "script": w.script,
                "script_exists": dir.join(&w.script).is_file(),
                "why": w.why,
            })
        })
        .collect();

    let mut sqlite_files: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .take(2_000)
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    let n = n.to_lowercase();
                    n.ends_with(".sqlite3") || n.ends_with(".sqlite") || n.ends_with(".db")
                })
                .collect()
        })
        .unwrap_or_default();
    sqlite_files.sort();
    sqlite_files.truncate(10);

    let project = detect_project(dir);
    let starters: Vec<Value> = project
        .starters
        .iter()
        .map(|s| {
            let existing = std::fs::read_to_string(dir.join(s.script())).ok();
            json!({
                "id": s.id,
                "label": s.label,
                "description": s.description,
                "commands": s.lines,
                "default_port": s.default_port,
                "run_mode": s.run_mode,
                "script": s.script(),
                "script_exists": existing.is_some(),
                "script_is_ours": existing.map(|t| t.contains(GENERATED_MARKER)).unwrap_or(false),
            })
        })
        .collect();

    let info = git::inspect(dir, "main");
    json!({
        "exists": true,
        "project": {"kind": project.kind, "label": project.label},
        "starters": starters,
        "sqlite_files": sqlite_files,
        "workers": workers,
        "suggested_name": dir.file_name().map(|n| n.to_string_lossy().into_owned()),
        "scripts": scripts,
        "detected": detected,
        "is_repo": info.is_repo,
        "branch": info.checked_out_branch,
        "remote_url": info.remote_url,
        "head_summary": info.head_summary,
    })
}
