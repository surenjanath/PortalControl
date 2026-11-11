//! Axum REST + WebSocket surface of the control plane.
//!
//! Besides target CRUD and execution it exposes deployments, Git details,
//! metrics, activity, webhooks and manager settings to the dashboard.

use crate::automation::{self, Cron};
use crate::db::{Target, TargetInput};
use crate::state::AppState;
use crate::{autostart, backup, git, health, net, runner, security};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

const INDEX_HTML: &str = include_str!("../web/index.html");

type AppResult = Result<Response, Response>;
type Params = Query<HashMap<String, String>>;

fn fail(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn internal(e: String) -> Response {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/targets", get(list_targets).post(create_target))
        .route(
            "/api/targets/{id}",
            get(get_target).put(update_target).delete(delete_target),
        )
        .route("/api/targets/{id}/run", post(run_target))
        .route("/api/targets/{id}/stop", post(stop_target))
        .route("/api/targets/{id}/restart", post(restart_target))
        .route("/api/targets/{id}/deploy", post(deploy_target))
        .route("/api/targets/{id}/logs", get(target_logs))
        .route("/api/targets/{id}/history", get(history).delete(clear_history))
        .route("/api/targets/{id}/scan", get(latest_scan).post(start_scan))
        .route("/api/targets/{id}/scans", get(list_scans))
        .route("/api/targets/{id}/git/refresh", post(git_refresh))
        .route("/api/targets/{id}/git/pull", post(git_pull))
        .route("/api/targets/{id}/git/log", get(git_log))
        .route("/api/targets/{id}/git/changes", get(git_changes))
        .route("/api/targets/{id}/health/check", post(health_check))
        .route("/api/targets/{id}/health/history", get(health_history))
        .route("/api/targets/{id}/metrics", get(target_metrics))
        .route(
            "/api/targets/{id}/webhook",
            post(webhook_enable).delete(webhook_disable),
        )
        .route("/api/webhooks/deploy/{token}", get(webhook_deploy).post(webhook_deploy))
        .route("/api/targets/{id}/commands", get(list_commands))
        .route("/api/targets/{id}/commands/stop", post(stop_command))
        .route("/api/targets/{id}/commands/{command}", post(run_command))
        .route("/api/targets/{id}/backups", get(list_backups).post(create_backup))
        .route("/api/targets/{id}/backups/{file}", axum::routing::delete(delete_backup))
        .route("/api/targets/{id}/backups/{file}/restore", post(restore_backup))
        .route("/api/export", get(export_config))
        .route("/api/import", post(import_config))
        .route("/api/runs", get(recent_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/activity", get(activity))
        .route("/api/cron", get(cron_preview))
        .route("/api/fs/inspect", get(fs_inspect))
        .route("/api/fs/launcher", post(fs_launcher))
        .route("/api/system", get(system))
        .route("/api/system/metrics", get(system_metrics))
        .route("/api/system/log", get(system_log))
        .route(
            "/api/system/autostart",
            post(autostart_install).delete(autostart_remove),
        )
        .route("/api/system/shutdown", post(shutdown))
        .route("/ws", get(ws_upgrade))
        .layer(middleware::from_fn(guard))
        .with_state(state)
}

/// Sandbox guard (PRD section 5). Because this server can execute scripts,
/// every request must be addressed to localhost / a private IP, and a browser
/// request must originate from the dashboard itself. This blocks DNS
/// rebinding and cross-site requests from pages on the public internet.
async fn guard(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !net::host_header_allowed(&host) {
        return fail(StatusCode::FORBIDDEN, "forbidden host");
    }
    if let Some(origin) = req.headers().get(ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if !net::origin_allowed(origin, &host) {
            return fail(StatusCode::FORBIDDEN, "cross-origin requests are not allowed");
        }
    }
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

async fn index() -> Response {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                CONTENT_SECURITY_POLICY,
                "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                 connect-src 'self' ws: wss:; img-src 'self' data:; frame-ancestors 'none'",
            ),
        ],
        INDEX_HTML,
    )
        .into_response()
}

// ------------------------------------------------------------- validation --

fn clean_input(mut t: TargetInput) -> Result<TargetInput, String> {
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
    Ok(t)
}

async fn load_target(state: &Arc<AppState>, id: i64) -> Result<Target, Response> {
    state
        .db
        .get_target(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "target not found"))
}

/// Kick off an immediate git inspection (with fetch) and health probe so a
/// new or edited target fills in without waiting for the periodic workers.
fn refresh_in_background(state: &Arc<AppState>, t: &Target) {
    let st = state.clone();
    let (id, dir, branch) = (t.id, t.working_directory.clone(), t.git_branch.clone());
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

async fn target_view(state: &Arc<AppState>, t: Target) -> Value {
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
        (
            r.get(&t.id).map(view),
            r.get(&runner::command_slot(t.id)).map(view),
        )
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
    let scan_running = state
        .scans_running
        .lock()
        .map(|s| s.contains(&t.id))
        .unwrap_or(false);
    let metrics = if running.is_some() {
        state.metrics.lock().ok().and_then(|m| m.latest(t.id))
    } else {
        None
    };
    let restart_pending = state
        .restart_queue
        .lock()
        .map(|q| q.iter().any(|(id, _)| *id == t.id))
        .unwrap_or(false);
    let scan = state.db.latest_scan(t.id, false).await.ok().flatten();
    let last_run = state
        .db
        .list_runs(t.id, 1)
        .await
        .ok()
        .and_then(|v| v.into_iter().next());

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
    v
}

// ---------------------------------------------------------------- targets --

async fn list_targets(State(state): State<Arc<AppState>>) -> AppResult {
    let targets = state.db.list_targets().await.map_err(internal)?;
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        out.push(target_view(&state, t).await);
    }
    Ok(ok(json!(out)))
}

async fn get_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    Ok(ok(target_view(&state, t).await))
}

async fn create_target(
    State(state): State<Arc<AppState>>,
    Json(input): Json<TargetInput>,
) -> AppResult {
    let input = clean_input(input).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let id = state.db.insert_target(input).await.map_err(internal)?;
    let t = load_target(&state, id).await?;
    refresh_in_background(&state, &t);
    state.activity("info", "target", Some(id), format!("Target '{}' registered", t.name));
    state.emit(json!({"type": "targets_changed"}));
    Ok((StatusCode::CREATED, Json(target_view(&state, t).await)).into_response())
}

async fn update_target(
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
    Ok(ok(target_view(&state, t).await))
}

async fn delete_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
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

fn run_response(result: Result<i64, String>) -> AppResult {
    match result {
        Ok(run_id) => Ok((StatusCode::ACCEPTED, Json(json!({"run_id": run_id}))).into_response()),
        Err(e) if e.contains("already running") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) if e.contains("git pull failed") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

/// An operator action clears the crash-loop counter, re-arming auto-restart.
fn reset_restart_state(state: &Arc<AppState>, id: i64) {
    if let Ok(mut m) = state.restart_log.lock() {
        m.remove(&id);
    }
    if let Ok(mut q) = state.restart_queue.lock() {
        q.retain(|(tid, _)| *tid != id);
    }
}

async fn run_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    run_response(runner::start_run(&state, &t, "MANUAL").await)
}

async fn stop_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let had_pending_restart = state
        .restart_queue
        .lock()
        .map(|q| q.iter().any(|(tid, _)| *tid == id))
        .unwrap_or(false);
    reset_restart_state(&state, id);
    if runner::stop_run(&state, id).await {
        Ok(ok(json!({"stopping": true})))
    } else if had_pending_restart {
        Ok(ok(json!({"stopping": false, "restart_cancelled": true})))
    } else {
        Err(fail(StatusCode::CONFLICT, "target is not running"))
    }
}

async fn restart_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    run_response(runner::restart(&state, &t, "RESTART").await)
}

/// Deploy = fast-forward to origin (unless `?pull=0`) and run the script.
async fn deploy_target(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> AppResult {
    let t = load_target(&state, id).await?;
    reset_restart_state(&state, id);
    let pull = q.get("pull").map(|v| v != "0" && v != "false").unwrap_or(true);
    run_response(automation::deploy(&state, &t, "MANUAL", pull).await)
}

async fn target_logs(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let live = {
        let r = state.running.lock().await;
        r.get(&id)
            .map(|h| (h.run_id, h.tail.lock().map(|t| t.joined()).unwrap_or_default()))
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

async fn history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let runs = state.db.list_runs(id, 100).await.map_err(internal)?;
    Ok(ok(json!(runs)))
}

async fn clear_history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let removed = state.db.clear_runs(id).await.map_err(internal)?;
    state.activity("warn", "target", Some(id), format!("Deployment history cleared ({removed} run(s))"));
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"removed": removed})))
}

async fn recent_runs(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let runs = state.db.list_recent_runs(limit).await.map_err(internal)?;
    Ok(ok(json!(runs)))
}

async fn get_run(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
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

// ------------------------------------------------- security / git / health --

async fn start_scan(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match security::start_scan(&state, t) {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(json!({"scanning": true}))).into_response()),
        Err(e) if e.contains("already running") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

async fn latest_scan(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let scan = state.db.latest_scan(id, true).await.map_err(internal)?;
    Ok(ok(json!(scan)))
}

async fn list_scans(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let scans = state.db.list_scans(id, 30).await.map_err(internal)?;
    Ok(ok(json!(scans)))
}

async fn git_refresh(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let info = git::refresh_target(&state, id, t.working_directory, t.git_branch, true).await;
    Ok(ok(json!(info)))
}

async fn git_pull(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match automation::pull(&state, &t).await {
        Ok(outcome) => Ok(ok(json!(outcome))),
        Err(e) => Err(fail(StatusCode::CONFLICT, e)),
    }
}

async fn git_log(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let commits = tokio::task::spawn_blocking(move || {
        git::recent_commits(std::path::Path::new(&t.working_directory), &t.git_branch, 30)
    })
    .await
    .map_err(|e| internal(e.to_string()))?
    .unwrap_or_default();
    Ok(ok(json!(commits)))
}

async fn git_changes(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let files = tokio::task::spawn_blocking(move || {
        git::changed_files(std::path::Path::new(&t.working_directory), 200)
    })
    .await
    .map_err(|e| internal(e.to_string()))?
    .unwrap_or_default();
    Ok(ok(json!(files)))
}

async fn health_check(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match t.health_url {
        Some(url) => Ok(ok(json!(health::probe_target(&state, id, &url).await))),
        None => Err(fail(StatusCode::BAD_REQUEST, "target has no health URL")),
    }
}

async fn health_history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let samples: Vec<_> = state
        .health_history
        .lock()
        .map(|m| m.get(&id).map(|r| r.iter().cloned().collect()).unwrap_or_default())
        .unwrap_or_default();
    Ok(ok(json!({"samples": samples, "probe_interval_secs": 15})))
}

async fn target_metrics(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let body = {
        let m = state.metrics.lock().unwrap_or_else(|p| p.into_inner());
        let samples: Vec<_> = m.targets.get(&id).map(|r| r.iter().cloned().collect()).unwrap_or_default();
        json!({"samples": samples, "cpus": m.cpus, "mem_total": m.mem_total, "interval_secs": 5})
    };
    Ok(ok(body))
}

// --------------------------------------------------------------- webhooks --

async fn webhook_enable(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let token = automation::new_token();
    state
        .db
        .set_webhook_token(id, Some(token.clone()))
        .await
        .map_err(internal)?;
    state.activity("info", "target", Some(id), "Deploy webhook URL generated");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_token": token})))
}

async fn webhook_disable(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    state.db.set_webhook_token(id, None).await.map_err(internal)?;
    state.activity("info", "target", Some(id), "Deploy webhook disabled");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_token": null})))
}

/// Trigger a deploy from outside the dashboard (a Git server hook, a script,
/// `curl`). The unguessable token in the URL is the credential.
async fn webhook_deploy(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    Query(q): Params,
) -> AppResult {
    if token.len() < 16 {
        return Err(fail(StatusCode::NOT_FOUND, "unknown webhook"));
    }
    let Some(t) = state.db.target_by_webhook(token).await.map_err(internal)? else {
        return Err(fail(StatusCode::NOT_FOUND, "unknown webhook"));
    };
    let pull = q.get("pull").map(|v| v != "0" && v != "false").unwrap_or(true);
    let result = automation::deploy(&state, &t, "WEBHOOK", pull).await;
    if let Err(e) = &result {
        state.activity("error", "deploy", Some(t.id), format!("Webhook deploy rejected: {e}"));
    }
    run_response(result)
}

// ------------------------------------------------------- activity / tools --

async fn activity(State(state): State<Arc<AppState>>) -> AppResult {
    let events: Vec<Value> = state
        .activity
        .lock()
        .map(|ring| ring.iter().rev().cloned().collect())
        .unwrap_or_default();
    Ok(ok(json!(events)))
}

async fn cron_preview(Query(q): Params) -> AppResult {
    let expr = q.get("expr").map(String::as_str).unwrap_or("");
    match automation::preview(expr, 5) {
        Ok(next) => Ok(ok(json!({"valid": true, "next": next}))),
        Err(e) => Ok(ok(json!({"valid": false, "error": e}))),
    }
}

/// Look at a directory before registering it: which scripts it holds, whether
/// it is a Git repository and what kind of project it appears to be.
async fn fs_inspect(Query(q): Params) -> AppResult {
    let raw = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    let dir = std::path::PathBuf::from(&raw);
    if raw.is_empty() || !dir.is_absolute() {
        return Ok(ok(json!({"exists": false, "error": "enter an absolute path"})));
    }
    let report = tokio::task::spawn_blocking(move || inspect_dir(&dir))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(report))
}

fn inspect_dir(dir: &std::path::Path) -> Value {
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
    let detected: Vec<&str> = markers
        .iter()
        .filter(|(file, _)| dir.join(file).exists())
        .map(|(_, label)| *label)
        .collect();

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

    let info = git::inspect(dir, "main");
    json!({
        "exists": true,
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

// --------------------------------------------------------- quick commands --

/// A maintenance command PortalControl can run for a target. The program and
/// its arguments are fixed here; nothing typed in the dashboard is executed.
struct QuickCommand {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    args: &'static [&'static str],
    /// True when the command changes the database, files or environment.
    changes: bool,
    /// Take a database backup first when the target has backups enabled.
    backup_first: bool,
}

const DJANGO_COMMANDS: &[QuickCommand] = &[
    QuickCommand { id: "check", label: "System check", description: "Validate the project configuration.", args: &["manage.py", "check"], changes: false, backup_first: false },
    QuickCommand { id: "showmigrations", label: "Show migrations", description: "List every migration and whether it has been applied.", args: &["manage.py", "showmigrations"], changes: false, backup_first: false },
    QuickCommand { id: "pending", label: "Check for model changes", description: "Report model changes that have no migration yet. Nothing is written.", args: &["manage.py", "makemigrations", "--check", "--dry-run"], changes: false, backup_first: false },
    QuickCommand { id: "migrate", label: "Apply migrations", description: "Run pending database migrations.", args: &["manage.py", "migrate", "--noinput"], changes: true, backup_first: true },
    QuickCommand { id: "collectstatic", label: "Collect static files", description: "Copy static files into STATIC_ROOT.", args: &["manage.py", "collectstatic", "--noinput"], changes: true, backup_first: false },
    QuickCommand { id: "clearsessions", label: "Clear expired sessions", description: "Delete expired rows from the session store.", args: &["manage.py", "clearsessions"], changes: true, backup_first: false },
];

const PYTHON_COMMANDS: &[QuickCommand] = &[
    QuickCommand { id: "pip-install", label: "Install requirements", description: "pip install -r requirements.txt, into the project's virtualenv if it has one, otherwise into the Python on PATH.", args: &["-m", "pip", "install", "-r", "requirements.txt"], changes: true, backup_first: false },
    QuickCommand { id: "pip-outdated", label: "List outdated packages", description: "Show installed packages that have newer releases.", args: &["-m", "pip", "list", "--outdated"], changes: false, backup_first: false },
];

fn detect_commands(dir: &std::path::Path) -> Vec<&'static QuickCommand> {
    let mut out: Vec<&'static QuickCommand> = Vec::new();
    if dir.join("manage.py").is_file() {
        out.extend(DJANGO_COMMANDS);
    }
    if dir.join("requirements.txt").is_file() {
        out.extend(PYTHON_COMMANDS);
    }
    out
}

async fn list_commands(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = std::path::PathBuf::from(&t.working_directory);
    let python = project_python(&dir);
    let commands: Vec<Value> = detect_commands(&dir)
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "description": c.description,
                "command": format!("{python} {}", c.args.join(" ")),
                "changes": c.changes,
                "backup_first": c.backup_first && t.backup_on_deploy,
            })
        })
        .collect();
    Ok(ok(json!({"python": python, "commands": commands})))
}

async fn run_command(
    State(state): State<Arc<AppState>>,
    Path((id, command)): Path<(i64, String)>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = std::path::PathBuf::from(&t.working_directory);
    let Some(cmd) = detect_commands(&dir).into_iter().find(|c| c.id == command) else {
        return Err(fail(StatusCode::NOT_FOUND, "this command is not available for the target"));
    };
    if cmd.backup_first && t.backup_on_deploy {
        backup::before_run(&state, &t).await;
    }
    let args: Vec<String> = cmd.args.iter().map(|a| a.to_string()).collect();
    run_response(runner::start_command(&state, &t, cmd.label, &project_python(&dir), &args).await)
}

async fn stop_command(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    if runner::stop_run(&state, runner::command_slot(id)).await {
        Ok(ok(json!({"stopping": true})))
    } else {
        Err(fail(StatusCode::CONFLICT, "no command is running"))
    }
}

// ---------------------------------------------------------------- backups --

fn data_dir(state: &Arc<AppState>) -> Result<std::path::PathBuf, Response> {
    state
        .data_dir
        .get()
        .cloned()
        .ok_or_else(|| internal("data directory is not configured".into()))
}

async fn list_backups(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let shown = backup::dir_for(&dir, id).display().to_string();
    let list = tokio::task::spawn_blocking(move || backup::list(&dir, id))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(json!({"backups": list, "directory": shown, "keep": backup::KEEP})))
}

async fn create_backup(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let made = tokio::task::spawn_blocking(move || backup::create(&dir, &t))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity("ok", "backup", Some(id), format!("Database backup created: {}", made.file));
    Ok((StatusCode::CREATED, Json(json!(made))).into_response())
}

async fn delete_backup(
    State(state): State<Arc<AppState>>,
    Path((id, file)): Path<(i64, String)>,
) -> AppResult {
    load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let name = file.clone();
    tokio::task::spawn_blocking(move || backup::delete(&dir, id, &name))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    Ok(ok(json!({"deleted": file})))
}

async fn restore_backup(
    State(state): State<Arc<AppState>>,
    Path((id, file)): Path<(i64, String)>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    {
        let r = state.running.lock().await;
        if r.contains_key(&id) || r.contains_key(&runner::command_slot(id)) {
            return Err(fail(
                StatusCode::CONFLICT,
                "stop the target (and any running command) before restoring its database",
            ));
        }
    }
    let dir = data_dir(&state)?;
    let name = file.clone();
    let safety = tokio::task::spawn_blocking(move || backup::restore(&dir, &t, &name))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity(
        "warn",
        "backup",
        Some(id),
        format!("Database restored from {file}; the previous state was saved as {}", safety.file),
    );
    Ok(ok(json!({"restored": file, "safety_backup": safety})))
}

// --------------------------------------------------------- export / import --

/// Every target's configuration as portable JSON (no history, no tokens).
async fn export_config(State(state): State<Arc<AppState>>) -> AppResult {
    let targets = state.db.list_targets().await.map_err(internal)?;
    let items: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "name": t.name, "description": t.description, "project": t.project,
                "working_directory": t.working_directory, "script_name": t.script_name,
                "git_branch": t.git_branch, "health_url": t.health_url, "run_mode": t.run_mode,
                "autostart": t.autostart, "timeout_secs": t.timeout_secs, "env_vars": t.env_vars,
                "schedule": t.schedule, "auto_deploy": t.auto_deploy, "auto_restart": t.auto_restart,
                "db_file": t.db_file, "backup_on_deploy": t.backup_on_deploy,
            })
        })
        .collect();
    Ok(ok(json!({
        "portalcontrol_export": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "exported_at": crate::util::fmt_utc(crate::util::now_secs()),
        "targets": items,
    })))
}

#[derive(serde::Deserialize)]
struct ImportRequest {
    targets: Vec<TargetInput>,
}

/// Register the targets of an export. A target whose folder and script are
/// already registered is skipped; one that fails validation (for example the
/// folder does not exist on this machine) is reported, not created.
async fn import_config(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportRequest>,
) -> AppResult {
    if req.targets.len() > 500 {
        return Err(fail(StatusCode::BAD_REQUEST, "an import is limited to 500 targets"));
    }
    let existing = state.db.list_targets().await.map_err(internal)?;
    let mut seen: Vec<(String, String)> = existing
        .iter()
        .map(|t| (t.working_directory.to_lowercase(), t.script_name.to_lowercase()))
        .collect();
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

// ---------------------------------------------------- companion processes --

/// A background process a project needs besides its main script, e.g. a
/// Django-Q cluster or a Celery worker.
#[derive(Debug, Clone, PartialEq)]
struct Worker {
    kind: &'static str,
    label: &'static str,
    /// The command the launcher runs, relative to the project directory.
    command: String,
    /// File name of the launcher script PortalControl can create.
    script: String,
    /// What gave it away, shown to the operator.
    why: String,
}

fn read_small(path: &std::path::Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() <= 512 * 1024 => {
            std::fs::read_to_string(path).unwrap_or_default().to_lowercase()
        }
        _ => String::new(),
    }
}

/// The Python interpreter a launcher should use: the project's own
/// virtualenv when it has one for this platform, else `python` from PATH.
fn project_python(dir: &std::path::Path) -> String {
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
    "python".to_string()
}

/// Look for task-queue workers that belong to a Django project: Django-Q
/// (`manage.py qcluster`) and Celery (worker, and beat when it is configured).
fn detect_workers(dir: &std::path::Path) -> Vec<Worker> {
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
        });
        if let Some(source) = find(&settings, &["django_celery_beat", "beat_schedule"]) {
            out.push(Worker {
                kind: "celery_beat",
                label: "Celery beat",
                command: format!("{python} -m celery -A {app} beat --loglevel=info"),
                script: format!("portalcontrol_celery_beat.{ext}"),
                why: format!("beat schedule found in {source}"),
            });
        }
    }
    out
}

fn launcher_body(worker: &Worker) -> String {
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
fn create_launcher(dir: &std::path::Path, kind: &str) -> Result<(String, bool), String> {
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
struct LauncherRequest {
    dir: String,
    kind: String,
}

async fn fs_launcher(
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

// ----------------------------------------------------------------- system --

async fn system(State(state): State<Arc<AppState>>) -> AppResult {
    let (shell_binary, shell_switch) = runner::shell();
    let active_runs = state.running.lock().await.len();
    let targets = state.db.list_targets().await.map(|t| t.len()).unwrap_or(0);
    let host = {
        let m = state.metrics.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "name": m.host_name,
            "os_version": m.os_version,
            "cpus": m.cpus,
            "mem_total": m.mem_total,
            "latest": m.host.back(),
        })
    };
    let now = chrono::Local::now();
    Ok(ok(json!({
        "name": "PortalControl-Core",
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "shell": format!("{shell_binary} {shell_switch}"),
        "pid": std::process::id(),
        "bind": state.bind.to_string(),
        "uptime_secs": state.started.elapsed().as_secs(),
        "targets": targets,
        "active_runs": active_runs,
        "active_scans": state.scans_running.lock().map(|s| s.len()).unwrap_or(0),
        "ws_clients": state.ws_clients.load(Ordering::Relaxed),
        "worker_threads": crate::worker_threads(),
        "host": host,
        "data_dir": state.data_dir.get().map(|p| p.display().to_string()),
        "executable": std::env::current_exe().ok().map(|p| p.display().to_string()),
        "autostart": autostart::status().ok().flatten(),
        "utc_offset_minutes": now.offset().local_minus_utc() / 60,
        "intervals": {
            "health_probe_secs": 15,
            "git_inspect_secs": 30,
            "git_fetch_secs": 300,
            "metrics_secs": 5,
        },
    })))
}

async fn system_metrics(State(state): State<Arc<AppState>>) -> AppResult {
    let body = {
        let m = state.metrics.lock().unwrap_or_else(|p| p.into_inner());
        let samples: Vec<_> = m.host.iter().cloned().collect();
        json!({"samples": samples, "cpus": m.cpus, "mem_total": m.mem_total, "interval_secs": 5})
    };
    Ok(ok(body))
}

/// Last part of a text file, starting at a line boundary.
fn tail_file(path: &std::path::Path, max_bytes: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or(text)
    } else {
        text
    })
}

async fn system_log(State(state): State<Arc<AppState>>) -> AppResult {
    let Some(dir) = state.data_dir.get().cloned() else {
        return Ok(ok(json!({"path": null, "log": ""})));
    };
    let path = dir.join("portalcontrol.log");
    let shown = path.display().to_string();
    let log = tokio::task::spawn_blocking(move || tail_file(&path, 48 * 1024))
        .await
        .map_err(|e| internal(e.to_string()))?
        .unwrap_or_default();
    Ok(ok(json!({"path": shown, "log": log})))
}

async fn autostart_install(State(state): State<Arc<AppState>>) -> AppResult {
    let args = state.serve_args.get().cloned().unwrap_or_default();
    let result = tokio::task::spawn_blocking(move || autostart::install(&args))
        .await
        .map_err(|e| internal(e.to_string()))?;
    match result {
        Ok(message) => {
            state.activity("info", "system", None, "Start at login enabled");
            Ok(ok(json!({"message": message, "autostart": autostart::status().ok().flatten()})))
        }
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

async fn autostart_remove(State(state): State<Arc<AppState>>) -> AppResult {
    let result = tokio::task::spawn_blocking(autostart::uninstall)
        .await
        .map_err(|e| internal(e.to_string()))?;
    match result {
        Ok(message) => {
            state.activity("info", "system", None, "Start at login disabled");
            Ok(ok(json!({"message": message, "autostart": null})))
        }
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

async fn shutdown(State(state): State<Arc<AppState>>) -> AppResult {
    state.shutdown.notify_one();
    Ok(ok(json!({"shutting_down": true})))
}

// -------------------------------------------------------------- websocket --

async fn ws_upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, state))
}

/// Forward the event stream to one dashboard. The broadcast channel is
/// bounded: a client that cannot keep up skips lines instead of making the
/// server buffer them.
async fn ws_session(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.events.subscribe();
    state.ws_clients.fetch_add(1, Ordering::Relaxed);
    let hello = json!({"type": "hello", "version": env!("CARGO_PKG_VERSION")}).to_string();
    if socket.send(Message::Text(hello.into())).await.is_ok() {
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        let note = json!({"type": "lagged", "skipped": skipped}).to_string();
                        if socket.send(Message::Text(note.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    state.ws_clients.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(dir: &std::path::Path, script: &str) -> TargetInput {
        TargetInput {
            name: "  Claims Portal ".into(),
            working_directory: dir.to_string_lossy().into_owned(),
            script_name: script.into(),
            health_url: Some("  ".into()),
            run_mode: Some("service".into()),
            ..Default::default()
        }
    }

    #[test]
    fn input_is_normalised_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy.bat"), "@echo off\n").unwrap();

        let t = clean_input(input(dir.path(), "deploy.bat")).unwrap();
        assert_eq!(t.name, "Claims Portal");
        assert_eq!(t.git_branch.as_deref(), Some("main"));
        assert_eq!(t.run_mode.as_deref(), Some("SERVICE"));
        assert_eq!(t.health_url, None);
        assert_eq!(t.timeout_secs, Some(300));
        assert_eq!(t.project.as_deref(), Some("Default"));
        assert_eq!(t.schedule, None);
        assert_eq!(t.env_vars.as_deref(), Some(""));
        assert_eq!((t.auto_deploy, t.auto_restart), (Some(false), Some(false)));

        assert!(clean_input(input(dir.path(), "missing.bat")).is_err());
        assert!(clean_input(input(dir.path(), "..\\deploy.bat")).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.working_directory = "relative/dir".into();
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.run_mode = Some("DAEMON".into());
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.health_url = Some("ftp://nope".into());
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.timeout_secs = Some(0);
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.git_branch = Some("--upload-pack=x".into());
        assert!(clean_input(bad).is_err());
    }

    #[test]
    fn new_fields_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy.bat"), "@echo off\n").unwrap();

        let mut good = input(dir.path(), "deploy.bat");
        good.project = Some("  Insurance  ".into());
        good.schedule = Some(" 0 2 * * * ".into());
        good.env_vars = Some("DEBUG=1\r\nSECRET=abc\r\n".into());
        good.description = Some(" claims portal ".into());
        let t = clean_input(good).unwrap();
        assert_eq!(t.project.as_deref(), Some("Insurance"));
        assert_eq!(t.schedule.as_deref(), Some("0 2 * * *"));
        assert_eq!(t.env_vars.as_deref(), Some("DEBUG=1\nSECRET=abc"));
        assert_eq!(t.description.as_deref(), Some("claims portal"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.schedule = Some("every day".into());
        assert!(clean_input(bad).unwrap_err().contains("schedule"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.env_vars = Some("NOT VALID".into());
        assert!(clean_input(bad).unwrap_err().contains("environment"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.description = Some("x".repeat(501));
        assert!(clean_input(bad).is_err());
    }

    #[test]
    fn directory_inspection_finds_scripts_and_project_type() {
        let dir = tempfile::tempdir().unwrap();
        let (script, other) = if cfg!(windows) { ("deploy.bat", "deploy.sh") } else { ("deploy.sh", "deploy.bat") };
        std::fs::write(dir.path().join(script), "echo hi\n").unwrap();
        std::fs::write(dir.path().join(other), "echo hi\n").unwrap();
        std::fs::write(dir.path().join("manage.py"), "").unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts").join(script), "echo hi\n").unwrap();

        let v = inspect_dir(dir.path());
        assert_eq!(v["exists"], true);
        assert_eq!(v["scripts"], json!([script, format!("scripts/{script}")]));
        assert_eq!(v["detected"], json!(["Django"]));
        assert_eq!(v["is_repo"], false);

        assert_eq!(inspect_dir(&dir.path().join("nope"))["exists"], false);
    }

    #[test]
    fn django_workers_are_detected_and_launchers_created() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // Not a Django project: nothing is suggested.
        std::fs::write(p.join("requirements.txt"), "django-q2==1.7\ncelery==5.4\n").unwrap();
        assert!(detect_workers(p).is_empty());
        assert!(create_launcher(p, "qcluster").is_err());

        std::fs::write(p.join("manage.py"), "").unwrap();
        std::fs::create_dir(p.join("portal")).unwrap();
        std::fs::write(
            p.join("portal").join("settings.py"),
            "INSTALLED_APPS = ['django_q']\nQ_CLUSTER = {'name': 'portal'}\n",
        )
        .unwrap();
        let found = detect_workers(p);
        assert_eq!(found.len(), 1, "celery needs a celery.py: {found:?}");
        assert_eq!(found[0].kind, "qcluster");
        assert_eq!(found[0].command, "python manage.py qcluster");
        assert!(found[0].why.contains("portal/settings.py"));

        // Celery app module + beat schedule.
        std::fs::write(p.join("portal").join("celery.py"), "app = Celery('portal')\n").unwrap();
        std::fs::write(
            p.join("portal").join("settings.py"),
            "INSTALLED_APPS = ['django_q']\nCELERY_BEAT_SCHEDULE = {}\n",
        )
        .unwrap();
        let kinds: Vec<&str> = detect_workers(p).iter().map(|w| w.kind).collect();
        assert_eq!(kinds, vec!["qcluster", "celery", "celery_beat"]);
        assert!(detect_workers(p)[1].command.contains("-A portal worker"));

        // The project's own virtualenv is preferred.
        let (bin, exe) = if cfg!(windows) { ("Scripts", "python.exe") } else { ("bin", "python") };
        std::fs::create_dir_all(p.join(".venv").join(bin)).unwrap();
        std::fs::write(p.join(".venv").join(bin).join(exe), "").unwrap();
        assert!(detect_workers(p)[0].command.starts_with(".venv"));

        // Creating a launcher writes it once and never overwrites it.
        let (script, created) = create_launcher(p, "qcluster").unwrap();
        assert!(created);
        assert!(runner::validate_target_paths(&p.to_string_lossy(), &script).is_ok());
        let body = std::fs::read_to_string(p.join(&script)).unwrap();
        assert!(body.contains("manage.py qcluster") && body.contains("Created by PortalControl"));
        std::fs::write(p.join(&script), "edited by hand").unwrap();
        assert_eq!(create_launcher(p, "qcluster").unwrap(), (script.clone(), false));
        assert_eq!(std::fs::read_to_string(p.join(&script)).unwrap(), "edited by hand");
        assert!(create_launcher(p, "bogus").is_err());

        let v = inspect_dir(p);
        assert_eq!(v["workers"][0]["kind"], "qcluster");
        assert_eq!(v["workers"][0]["script_exists"], true);
        assert_eq!(v["workers"][1]["script_exists"], false);
    }

    #[test]
    fn log_tail_starts_on_a_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, "first line\nsecond line\nthird line\n").unwrap();
        assert_eq!(tail_file(&path, 1_000).unwrap(), "first line\nsecond line\nthird line\n");
        assert_eq!(tail_file(&path, 15).unwrap(), "third line\n");
    }
}
