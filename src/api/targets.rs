//! Target CRUD; running, stopping, restarting, deploying and rolling back; project
//! start and stop; logs and run history.

use super::*;

pub(super) async fn list_targets(State(state): State<Arc<AppState>>) -> AppResult {
    let targets = state.db.list_targets().await.map_err(internal)?;
    let mut extras = load_extras(&state, &targets).await;
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        out.push(target_view(&state, t, &mut extras).await);
    }
    Ok(ok(json!(out)))
}

pub(super) async fn get_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    Ok(ok(one_target_view(&state, t).await))
}

pub(super) async fn create_target(
    State(state): State<Arc<AppState>>,
    Json(input): Json<TargetInput>,
) -> AppResult {
    let input = clean_input(input).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let id = state.db.insert_target(input).await.map_err(internal)?;
    let t = load_target(&state, id).await?;
    refresh_in_background(&state, &t);
    state.activity("info", "target", Some(id), format!("Target '{}' registered", t.name));
    state.emit(json!({"type": "targets_changed"}));
    Ok((StatusCode::CREATED, Json(one_target_view(&state, t).await)).into_response())
}

pub(super) async fn update_target(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<TargetInput>,
) -> AppResult {
    load_target(&state, id).await?;
    let input = clean_input(input).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.db.update_target(id, input).await.map_err(internal)?;
    let t = load_target(&state, id).await?;
    refresh_in_background(&state, &t);
    if let Ok(mut m) = state.auto_deploy_failed.lock() {
        m.remove(&id);
    }
    state.activity("info", "target", Some(id), "Configuration saved");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(one_target_view(&state, t).await))
}

pub(super) async fn delete_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    if let Ok(mut q) = state.restart_queue.lock() {
        q.retain(|(tid, _)| *tid != id);
    }
    let stopping_command = runner::stop_run(&state, runner::command_slot(id)).await;
    if runner::stop_run(&state, id).await || stopping_command {
        // Give the supervisor a moment to reap the process tree first.
        for _ in 0..50 {
            let r = state.running.lock().await;
            if !r.contains_key(&id) && !r.contains_key(&runner::command_slot(id)) {
                break;
            }
            drop(r);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    state.db.delete_target(id).await.map_err(internal)?;
    if let Ok(mut m) = state.health.lock() {
        m.remove(&id);
    }
    if let Ok(mut m) = state.health_history.lock() {
        m.remove(&id);
    }
    if let Ok(mut m) = state.git.lock() {
        m.remove(&id);
    }
    if let Ok(mut m) = state.metrics.lock() {
        m.targets.remove(&id);
    }
    state.activity("warn", "target", None, format!("Target '{}' deleted", t.name));
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"deleted": id})))
}

// -------------------------------------------------------------- execution --

pub(super) fn run_response(result: Result<i64, String>) -> AppResult {
    match result {
        Ok(run_id) => Ok((StatusCode::ACCEPTED, Json(json!({"run_id": run_id}))).into_response()),
        Err(e) if e.contains("already running") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) if e.contains("git pull failed") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

/// An operator action clears the crash-loop counter, re-arming auto-restart.
pub(super) fn reset_restart_state(state: &Arc<AppState>, id: i64) {
    if let Ok(mut m) = state.restart_log.lock() {
        m.remove(&id);
    }
    if let Ok(mut q) = state.restart_queue.lock() {
        q.retain(|(tid, _)| *tid != id);
    }
}

pub(super) async fn run_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    run_response(runner::start_run(&state, &t, "MANUAL").await)
}

pub(super) async fn stop_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let had_pending_restart =
        state.restart_queue.lock().map(|q| q.iter().any(|(tid, _)| *tid == id)).unwrap_or(false);
    reset_restart_state(&state, id);
    if runner::stop_run(&state, id).await {
        Ok(ok(json!({"stopping": true})))
    } else if had_pending_restart {
        Ok(ok(json!({"stopping": false, "restart_cancelled": true})))
    } else {
        Err(fail(StatusCode::CONFLICT, "target is not running"))
    }
}

pub(super) async fn restart_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    run_response(runner::restart(&state, &t, "RESTART").await)
}

/// Deploy = fast-forward to origin (unless `?pull=0`) and run the script.
pub(super) async fn deploy_target(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    let pull = q.get("pull").map(|v| v != "0" && v != "false").unwrap_or(true);
    run_response(automation::deploy(&state, &t, "MANUAL", pull).await)
}

#[derive(serde::Deserialize)]
pub(super) struct RollbackRequest {
    pub(super) sha: String,
}

/// Check out an earlier commit of the tracked branch and deploy it.
pub(super) async fn rollback_target(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<RollbackRequest>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    match automation::rollback(&state, &t, &req.sha).await {
        Err(e) if e.contains("rollback failed") => Err(fail(StatusCode::CONFLICT, e)),
        other => run_response(other),
    }
}

/// Start a project's stopped targets one after another, in start order.
pub(super) async fn project_start(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> AppResult {
    let members = automation::project_members(&state, &name).await.map_err(internal)?;
    if members.is_empty() {
        return Err(fail(StatusCode::NOT_FOUND, "no target belongs to this project"));
    }
    for t in &members {
        reset_restart_state(&state, t.id);
    }
    let order: Vec<&str> = members.iter().map(|t| t.name.as_str()).collect();
    match automation::start_project(&state, name.clone()) {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(json!({"starting": order}))).into_response()),
        Err(e) => Err(fail(StatusCode::CONFLICT, e)),
    }
}

pub(super) async fn project_stop(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> AppResult {
    for t in automation::project_members(&state, &name).await.map_err(internal)? {
        reset_restart_state(&state, t.id);
    }
    let stopped = automation::stop_project(&state, &name).await.map_err(internal)?;
    Ok(ok(json!({"stopping": stopped})))
}

pub(super) async fn target_logs(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let live = {
        let r = state.running.lock().await;
        r.get(&id).map(|h| (h.run_id, h.tail.lock().map(|t| t.joined()).unwrap_or_default()))
    };
    if let Some((run_id, logs)) = live {
        return Ok(ok(json!({"run_id": run_id, "live": true, "logs": logs})));
    }
    let last = state.db.list_runs(id, 1).await.map_err(internal)?;
    match last.first() {
        Some(r) => {
            let full = state.db.get_run(r.id).await.map_err(internal)?;
            Ok(ok(json!({
                "run_id": r.id,
                "live": false,
                "status": r.status,
                "logs": full.and_then(|f| f.captured_logs).unwrap_or_default(),
            })))
        }
        None => Ok(ok(json!({"run_id": null, "live": false, "logs": ""}))),
    }
}

pub(super) async fn history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let runs = state.db.list_runs(id, 100).await.map_err(internal)?;
    Ok(ok(json!(runs)))
}

pub(super) async fn clear_history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let removed = state.db.clear_runs(id).await.map_err(internal)?;
    state.activity("warn", "target", Some(id), format!("Deployment history cleared ({removed} run(s))"));
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"removed": removed})))
}

pub(super) async fn recent_runs(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let limit = q.get("limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(100).clamp(1, 500);
    let runs = state.db.list_recent_runs(limit).await.map_err(internal)?;
    Ok(ok(json!(runs)))
}

pub(super) async fn get_run(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    match state.db.get_run(id).await.map_err(internal)? {
        Some(mut r) => {
            // A run in progress has nothing persisted yet: serve the live tail.
            if r.status == "RUNNING" {
                let live = {
                    let running = state.running.lock().await;
                    running
                        .get(&r.target_id)
                        .filter(|h| h.run_id == r.id)
                        .map(|h| h.tail.lock().map(|t| t.joined()).unwrap_or_default())
                };
                if let Some(logs) = live {
                    r.captured_logs = Some(logs);
                }
            }
            Ok(ok(json!(r)))
        }
        None => Err(fail(StatusCode::NOT_FOUND, "run not found")),
    }
}
