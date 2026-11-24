//! Export and import of target configuration.

use super::*;

/// Every target's configuration as portable JSON (no history, no tokens).
/// Keep the names of environment variables but drop their values, so an
/// export can be shared without leaking passwords and API keys. Comments and
/// blank lines stay as they are.
pub(super) fn without_env_values(env: &str) -> String {
    env.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            match line.split_once('=') {
                Some((key, _)) if !trimmed.starts_with('#') => format!("{key}="),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `?env_values=1` includes environment variable values; by default only
/// their names are exported.
pub(super) async fn export_config(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let with_values = q.get("env_values").is_some_and(|v| v == "1" || v == "true");
    let targets = state.db.list_targets().await.map_err(internal)?;
    let items: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "name": t.name, "description": t.description, "project": t.project,
                "working_directory": t.working_directory, "script_name": t.script_name,
                "git_branch": t.git_branch, "health_url": t.health_url, "run_mode": t.run_mode,
                "autostart": t.autostart, "timeout_secs": t.timeout_secs,
                "env_vars": if with_values { t.env_vars.clone() } else { without_env_values(&t.env_vars) },
                "schedule": t.schedule, "auto_deploy": t.auto_deploy, "auto_restart": t.auto_restart,
                "db_file": t.db_file, "backup_on_deploy": t.backup_on_deploy,
                "start_order": t.start_order, "python_env": t.python_env, "git_dir": t.git_dir,
            })
        })
        .collect();
    Ok(ok(json!({
        "portalcontrol_export": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "exported_at": crate::util::fmt_utc(crate::util::now_secs()),
        "env_values_included": with_values,
        "targets": items,
    })))
}

#[derive(serde::Deserialize)]
pub(super) struct ImportRequest {
    pub(super) targets: Vec<TargetInput>,
}

/// Register the targets of an export. A target whose folder and script are
/// already registered is skipped; one that fails validation (for example the
/// folder does not exist on this machine) is reported, not created.
pub(super) async fn import_config(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportRequest>,
) -> AppResult {
    if req.targets.len() > 500 {
        return Err(fail(StatusCode::BAD_REQUEST, "an import is limited to 500 targets"));
    }
    let existing = state.db.list_targets().await.map_err(internal)?;
    let mut seen: Vec<(String, String)> =
        existing.iter().map(|t| (t.working_directory.to_lowercase(), t.script_name.to_lowercase())).collect();
    let (mut created, mut skipped, mut errors) = (0, 0, Vec::new());
    for input in req.targets {
        let name = input.name.clone();
        match clean_input(input) {
            Ok(clean) => {
                let key = (clean.working_directory.to_lowercase(), clean.script_name.to_lowercase());
                if seen.contains(&key) {
                    skipped += 1;
                    continue;
                }
                let id = state.db.insert_target(clean).await.map_err(internal)?;
                if let Ok(Some(t)) = state.db.get_target(id).await {
                    refresh_in_background(&state, &t);
                }
                seen.push(key);
                created += 1;
            }
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    if created > 0 {
        state.activity("info", "system", None, format!("Imported {created} target(s)"));
        state.emit(json!({"type": "targets_changed"}));
    }
    Ok(ok(json!({"created": created, "skipped": skipped, "errors": errors})))
}
