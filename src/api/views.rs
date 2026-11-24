//! Validation of target settings, and the JSON view of a target sent to the dashboard.

use super::*;

pub(super) fn clean_input(mut t: TargetInput) -> Result<TargetInput, String> {
    t.name = t.name.trim().to_string();
    if t.name.is_empty() || t.name.chars().count() > 100 {
        return Err("name is required (max 100 characters)".into());
    }
    t.working_directory = t.working_directory.trim().trim_matches('"').to_string();
    t.script_name = t.script_name.trim().to_string();
    runner::validate_target_paths(&t.working_directory, &t.script_name)?;

    let branch = t.git_branch.take().unwrap_or_default().trim().to_string();
    let branch = if branch.is_empty() { "main".to_string() } else { branch };
    if branch.len() > 200
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return Err("git branch name is invalid".into());
    }
    t.git_branch = Some(branch);

    let mode = t.run_mode.take().unwrap_or_default().trim().to_uppercase();
    t.run_mode = Some(match mode.as_str() {
        "" | "TASK" => "TASK".to_string(),
        "SERVICE" => "SERVICE".to_string(),
        _ => return Err("run_mode must be TASK or SERVICE".into()),
    });

    t.health_url = match t.health_url.take().map(|u| u.trim().to_string()) {
        Some(u) if !u.is_empty() => {
            health::parse_url(&u)?;
            Some(u)
        }
        _ => None,
    };

    let timeout = t.timeout_secs.unwrap_or(300);
    if !(1..=86_400).contains(&timeout) {
        return Err("timeout_secs must be between 1 and 86400".into());
    }
    t.timeout_secs = Some(timeout);

    let description = t.description.take().unwrap_or_default().trim().to_string();
    if description.chars().count() > 500 {
        return Err("description is limited to 500 characters".into());
    }
    t.description = Some(description);

    let project = t.project.take().unwrap_or_default().trim().to_string();
    if project.chars().count() > 60 {
        return Err("project name is limited to 60 characters".into());
    }
    t.project = Some(if project.is_empty() { "Default".to_string() } else { project });

    let env = t.env_vars.take().unwrap_or_default().replace("\r\n", "\n");
    let env = env.trim().to_string();
    runner::parse_env(&env).map_err(|e| format!("environment variables: {e}"))?;
    t.env_vars = Some(env);

    t.schedule = match t.schedule.take().map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => {
            Cron::parse(&s).map_err(|e| format!("schedule: {e}"))?;
            Some(s)
        }
        _ => None,
    };
    t.auto_deploy = Some(t.auto_deploy.unwrap_or(false));
    t.auto_restart = Some(t.auto_restart.unwrap_or(false));

    t.db_file = match t.db_file.take().map(|s| s.trim().replace('\\', "/")) {
        Some(s) if !s.is_empty() => {
            backup::resolve_db(&t.working_directory, &s)?;
            Some(s)
        }
        _ => None,
    };
    t.backup_on_deploy = Some(t.backup_on_deploy.unwrap_or(false) && t.db_file.is_some());
    let order = t.start_order.unwrap_or(0);
    if !(0..=999).contains(&order) {
        return Err("start order must be between 0 and 999".into());
    }
    t.start_order = Some(order);

    t.python_env = match t.python_env.take().map(|s| s.trim().trim_matches('"').to_string()) {
        Some(s) if s.eq_ignore_ascii_case(pyenv::AUTO) => Some(pyenv::AUTO.to_string()),
        Some(s) if !s.is_empty() => {
            if s.chars().count() > 500 {
                return Err("Python environment: the path is limited to 500 characters".into());
            }
            pyenv::resolve(std::path::Path::new(&t.working_directory), Some(&s))
                .map_err(|e| format!("Python environment: {e}"))?;
            Some(s)
        }
        _ => None,
    };

    t.git_dir = match t.git_dir.take().map(|s| s.trim().trim_matches('"').to_string()) {
        Some(s) if s.eq_ignore_ascii_case(git::AUTO) => Some(git::AUTO.to_string()),
        Some(s) if !s.is_empty() => {
            if s.chars().count() > 500 {
                return Err("Git repository: the path is limited to 500 characters".into());
            }
            git::check_repo_dir(&t.working_directory, &s).map_err(|e| format!("Git repository: {e}"))?;
            Some(s)
        }
        _ => None,
    };
    Ok(t)
}

pub(super) async fn load_target(state: &Arc<AppState>, id: i64) -> Result<Target, Response> {
    state
        .db
        .get_target(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "target not found"))
}

/// Kick off an immediate git inspection (with fetch) and health probe so a
/// new or edited target fills in without waiting for the periodic workers.
pub(super) fn refresh_in_background(state: &Arc<AppState>, t: &Target) {
    let st = state.clone();
    let (id, dir, branch) = (t.id, git::repo_dir(t), t.git_branch.clone());
    tokio::spawn(async move {
        git::refresh_target(&st, id, dir, branch, true).await;
    });
    match t.health_url.clone() {
        Some(url) => {
            let st = state.clone();
            tokio::spawn(async move {
                health::probe_target(&st, id, &url).await;
            });
        }
        None => {
            if let Ok(mut m) = state.health.lock() {
                m.remove(&id);
            }
            if let Ok(mut m) = state.health_history.lock() {
                m.remove(&id);
            }
        }
    }
}

pub(super) async fn target_view(state: &Arc<AppState>, t: Target, extras: &mut Extras) -> Value {
    let (running, command_running) = {
        let r = state.running.lock().await;
        let view = |h: &crate::state::RunHandle| {
            json!({
                "run_id": h.run_id,
                "pid": h.pid,
                "uptime_secs": h.started.elapsed().as_secs(),
                "started_ms": h.started_ms,
                "triggered_by": h.triggered_by,
            })
        };
        (r.get(&t.id).map(view), r.get(&runner::command_slot(t.id)).map(view))
    };
    let health = state.health.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let (health_recent, uptime_pct) = state
        .health_history
        .lock()
        .ok()
        .and_then(|m| {
            m.get(&t.id).map(|ring| {
                let up = ring.iter().filter(|s| s.status != "DOWN").count();
                let pct = if ring.is_empty() {
                    None
                } else {
                    Some((up as f64 * 1000.0 / ring.len() as f64).round() / 10.0)
                };
                let recent: Vec<_> = ring.iter().rev().take(40).rev().cloned().collect();
                (json!(recent), pct)
            })
        })
        .unwrap_or((json!([]), None));
    let git = state.git.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let scan_running = state.scans_running.lock().map(|s| s.contains(&t.id)).unwrap_or(false);
    let metrics =
        if running.is_some() { state.metrics.lock().ok().and_then(|m| m.latest(t.id)) } else { None };
    let restart_pending =
        state.restart_queue.lock().map(|q| q.iter().any(|(id, _)| *id == t.id)).unwrap_or(false);
    let scan = extras.scans.remove(&t.id);
    let last_run = extras.last_runs.remove(&t.id);
    let repo_dir = extras.repo_dirs.remove(&t.id).unwrap_or_else(|| t.working_directory.clone());

    let mut v = serde_json::to_value(&t).unwrap_or_else(|_| json!({}));
    v["running"] = json!(running);
    v["command_running"] = json!(command_running);
    v["health"] = json!(health);
    v["health_recent"] = health_recent;
    v["uptime_pct"] = json!(uptime_pct);
    v["git"] = json!(git);
    v["scan"] = json!(scan);
    v["scan_running"] = json!(scan_running);
    v["last_run"] = json!(last_run);
    v["metrics"] = json!(metrics);
    v["restart_pending"] = json!(restart_pending);
    v["next_run"] = json!(automation::next_run(t.schedule.as_deref()));
    v["env_count"] = json!(runner::parse_env(&t.env_vars).map(|e| e.len()).unwrap_or(0));
    v["python"] = extras.python.remove(&t.id).unwrap_or(Value::Null);
    // The folder the Git state is read from (see `git::repo_dir`).
    v["repo_dir"] = json!(repo_dir);
    v
}

/// What target views need from the database and the disk. It is loaded for
/// every target in one go: the dashboard asks for the whole list on each
/// refresh, and one query per target made that slow with many targets.
#[derive(Default)]
pub(super) struct Extras {
    pub(super) scans: HashMap<i64, crate::db::ScanReport>,
    pub(super) last_runs: HashMap<i64, crate::db::RunRecord>,
    pub(super) python: HashMap<i64, Value>,
    pub(super) repo_dirs: HashMap<i64, String>,
}

pub(super) async fn load_extras(state: &Arc<AppState>, targets: &[Target]) -> Extras {
    let (scans, last_runs) = tokio::join!(state.db.latest_scan_summaries(), state.db.last_runs());
    // Looking for Python environments and repositories touches the disk: off
    // the async threads.
    let wanted: Vec<Target> = targets.to_vec();
    let (python, repo_dirs) = tokio::task::spawn_blocking(move || {
        let python = wanted
            .iter()
            .map(|t| {
                (t.id, pyenv::describe(std::path::Path::new(&t.working_directory), t.python_env.as_deref()))
            })
            .collect();
        let repos = wanted.iter().map(|t| (t.id, git::repo_dir(t))).collect();
        (python, repos)
    })
    .await
    .unwrap_or_default();
    Extras { scans: scans.unwrap_or_default(), last_runs: last_runs.unwrap_or_default(), python, repo_dirs }
}

/// The view of one target, for the routes that return a single one.
pub(super) async fn one_target_view(state: &Arc<AppState>, t: Target) -> Value {
    let mut extras = load_extras(state, std::slice::from_ref(&t)).await;
    target_view(state, t, &mut extras).await
}
