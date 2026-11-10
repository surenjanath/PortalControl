//! Axum REST + WebSocket surface of the control plane.

use crate::db::{Target, TargetInput};
use crate::state::AppState;
use crate::{git, health, net, runner, security};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

const INDEX_HTML: &str = include_str!("../web/index.html");

type AppResult = Result<Response, Response>;

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
        .route("/api/targets/{id}/logs", get(target_logs))
        .route("/api/targets/{id}/history", get(history))
        .route("/api/targets/{id}/scan", get(latest_scan).post(start_scan))
        .route("/api/targets/{id}/git/refresh", post(git_refresh))
        .route("/api/targets/{id}/health/check", post(health_check))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/system", get(system))
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
/// new or edited card fills in without waiting for the periodic workers.
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
        }
    }
}

async fn target_view(state: &Arc<AppState>, t: Target) -> Value {
    let running = {
        let r = state.running.lock().await;
        r.get(&t.id).map(|h| {
            json!({"run_id": h.run_id, "pid": h.pid, "uptime_secs": h.started.elapsed().as_secs()})
        })
    };
    let health = state.health.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let git = state.git.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let scan_running = state
        .scans_running
        .lock()
        .map(|s| s.contains(&t.id))
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
    v["health"] = json!(health);
    v["git"] = json!(git);
    v["scan"] = json!(scan);
    v["scan_running"] = json!(scan_running);
    v["last_run"] = json!(last_run);
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
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(target_view(&state, t).await))
}

async fn delete_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    if runner::stop_run(&state, id).await {
        // Give the supervisor a moment to reap the process tree first.
        for _ in 0..50 {
            if !state.running.lock().await.contains_key(&id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    state.db.delete_target(id).await.map_err(internal)?;
    if let Ok(mut m) = state.health.lock() {
        m.remove(&id);
    }
    if let Ok(mut m) = state.git.lock() {
        m.remove(&id);
    }
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"deleted": id})))
}

// -------------------------------------------------------------- execution --

async fn run_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match runner::start_run(&state, &t, "MANUAL").await {
        Ok(run_id) => Ok((StatusCode::ACCEPTED, Json(json!({"run_id": run_id}))).into_response()),
        Err(e) if e.contains("already running") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

async fn stop_target(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    if runner::stop_run(&state, id).await {
        Ok(ok(json!({"stopping": true})))
    } else {
        Err(fail(StatusCode::CONFLICT, "target is not running"))
    }
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
    let runs = state.db.list_runs(id, 50).await.map_err(internal)?;
    Ok(ok(json!(runs)))
}

async fn get_run(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    match state.db.get_run(id).await.map_err(internal)? {
        Some(r) => Ok(ok(json!(r))),
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

async fn git_refresh(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let info = git::refresh_target(&state, id, t.working_directory, t.git_branch, true).await;
    Ok(ok(json!(info)))
}

async fn health_check(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match t.health_url {
        Some(url) => Ok(ok(json!(health::probe_target(&state, id, &url).await))),
        None => Err(fail(StatusCode::BAD_REQUEST, "target has no health URL")),
    }
}

// ----------------------------------------------------------------- system --

async fn system(State(state): State<Arc<AppState>>) -> AppResult {
    let (shell_binary, shell_switch) = runner::shell();
    let active_runs = state.running.lock().await.len();
    let targets = state.db.list_targets().await.map(|t| t.len()).unwrap_or(0);
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
    })))
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
            git_branch: None,
            health_url: Some("  ".into()),
            run_mode: Some("service".into()),
            autostart: None,
            timeout_secs: None,
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
}
