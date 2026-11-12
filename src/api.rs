//! Axum REST + WebSocket surface of the control plane.
//!
//! Besides target CRUD and execution it exposes deployments, Git details,
//! metrics, activity, webhooks and manager settings to the dashboard.

use crate::automation::{self, Cron};
use crate::db::{Target, TargetInput};
use crate::state::AppState;
use crate::{auth, autostart, backup, git, health, net, netcfg, pyenv, runner, security};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
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

// The dashboard is compiled into the binary: one page shell, one stylesheet
// and a handful of scripts, each small enough to read in one sitting.
const INDEX_HTML: &str = include_str!("../web/index.html");
const LOGIN_HTML: &str = include_str!("../web/login.html");
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const ASSETS: &[(&str, &str, &str)] = &[
    ("styles.css", CSS, include_str!("../web/styles.css")),
    ("icon.svg", "image/svg+xml", include_str!("../web/icon.svg")),
    ("js/core/core.js", JS, include_str!("../web/js/core/core.js")),
    ("js/core/components.js", JS, include_str!("../web/js/core/components.js")),
    ("js/core/router.js", JS, include_str!("../web/js/core/router.js")),
    ("js/pages/home.js", JS, include_str!("../web/js/pages/home.js")),
    ("js/pages/project.js", JS, include_str!("../web/js/pages/project.js")),
    ("js/pages/new.js", JS, include_str!("../web/js/pages/new.js")),
    ("js/target/target.js", JS, include_str!("../web/js/target/target.js")),
    ("js/target/config.js", JS, include_str!("../web/js/target/config.js")),
    ("js/target/python.js", JS, include_str!("../web/js/target/python.js")),
    ("js/target/network.js", JS, include_str!("../web/js/target/network.js")),
    ("js/target/tabs.js", JS, include_str!("../web/js/target/tabs.js")),
    ("js/target/metrics.js", JS, include_str!("../web/js/target/metrics.js")),
    ("js/settings/settings.js", JS, include_str!("../web/js/settings/settings.js")),
    ("js/settings/access.js", JS, include_str!("../web/js/settings/access.js")),
    ("js/settings/network.js", JS, include_str!("../web/js/settings/network.js")),
    ("js/settings/general.js", JS, include_str!("../web/js/settings/general.js")),
    ("js/settings/system.js", JS, include_str!("../web/js/settings/system.js")),
    ("js/pages/lists.js", JS, include_str!("../web/js/pages/lists.js")),
    ("js/pages/about.js", JS, include_str!("../web/js/pages/about.js")),
    ("js/core/app.js", JS, include_str!("../web/js/core/app.js")),
    ("js/login.js", JS, include_str!("../web/js/login.js")),
];
/// No inline scripts are allowed; inline `style` attributes are.
const CONTENT_POLICY: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self'; \
     connect-src 'self' ws: wss:; img-src 'self' data:; frame-ancestors 'none'";

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
        .route("/api/system/restart", post(restart))
        .route("/api/system/network", get(network_get).put(network_put))
        .route("/ws", get(ws_upgrade))
        .route("/assets/{*path}", get(asset))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/password", post(auth_password))
        .route("/api/targets/{id}/rollback", post(rollback_target))
        .route("/api/targets/{id}/metrics/history", get(metrics_history))
        .route("/api/targets/{id}/processes", get(target_processes))
        .route("/api/targets/{id}/network", get(target_network))
        .route("/api/fs/list", get(fs_list))
        .route("/api/fs/port", get(fs_port))
        .route("/api/fs/pyenv", get(fs_pyenv))
        .route("/api/fs/starter", post(fs_starter))
        .route("/api/projects/{name}/start", post(project_start))
        .route("/api/projects/{name}/stop", post(project_stop))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

fn html_page(body: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, CONTENT_POLICY),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn asset(Path(path): Path<String>) -> Response {
    match ASSETS.iter().find(|(name, _, _)| *name == path) {
        // `no-cache`: the browser revalidates, so a new build is picked up at once.
        Some((_, mime, body)) => ([(CONTENT_TYPE, *mime), (CACHE_CONTROL, "no-cache")], *body).into_response(),
        None => fail(StatusCode::NOT_FOUND, "no such asset"),
    }
}

// ------------------------------------------------------------------- auth --

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| auth::cookie_value(c, auth::SESSION_COOKIE))
        .map(str::to_string)
}

fn is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    if let Some(token) = headers.get(auth::CONTROL_HEADER).and_then(|v| v.to_str().ok()) {
        if state.auth.control_token_matches(token) {
            return true;
        }
    }
    session_token(headers).map(|t| state.auth.session_valid(&t)).unwrap_or(false)
}

/// Paths that must work without a session: static files, the sign-in calls
/// and deploy webhooks (whose URL token is their own credential).
fn is_public_path(path: &str) -> bool {
    path.starts_with("/assets/")
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        || path.starts_with("/api/webhooks/deploy/")
}

/// Marks requests that arrived through the HTTPS listener.
#[derive(Clone, Copy)]
pub struct ViaTls;
type Tls = Option<axum::Extension<ViaTls>>;

/// `secure`: the request came over HTTPS, so the browser must only ever send
/// the cookie back over HTTPS.
fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{}",
        auth::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

fn with_cookie(mut res: Response, cookie: String) -> Response {
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    res
}

async fn auth_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult {
    let password_set = state.auth.password_set();
    Ok(ok(json!({
        "password_set": password_set,
        "authenticated": !password_set || is_authenticated(&state, &headers),
        "min_password_chars": auth::MIN_PASSWORD_CHARS,
    })))
}

#[derive(serde::Deserialize)]
struct LoginRequest {
    password: String,
}

async fn verify(state: &Arc<AppState>, password: String) -> bool {
    let Some(hash) = state.auth.current_hash() else { return false };
    tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .unwrap_or(false)
}

async fn auth_login(
    State(state): State<Arc<AppState>>,
    tls: Tls,
    Json(req): Json<LoginRequest>,
) -> AppResult {
    if !state.auth.password_set() {
        return Ok(ok(json!({"authenticated": true})));
    }
    if let Some(secs) = state.auth.locked_for() {
        return Err(fail(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many wrong passwords; try again in {secs} seconds"),
        ));
    }
    if !verify(&state, req.password).await {
        state.auth.note_failure();
        state.activity("warn", "auth", None, "Failed sign-in attempt");
        return Err(fail(StatusCode::UNAUTHORIZED, "wrong password"));
    }
    state.auth.clear_failures();
    let token = state.auth.create_session();
    Ok(with_cookie(
        ok(json!({"authenticated": true})),
        session_cookie(&token, auth::SESSION_TTL.as_secs(), tls.is_some()),
    ))
}

async fn auth_logout(State(state): State<Arc<AppState>>, tls: Tls, headers: HeaderMap) -> AppResult {
    if let Some(token) = session_token(&headers) {
        state.auth.end_session(&token);
    }
    Ok(with_cookie(ok(json!({"authenticated": false})), session_cookie("", 0, tls.is_some())))
}

#[derive(serde::Deserialize)]
struct PasswordRequest {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new: String,
}

/// Set, change or (with an empty new password) remove the dashboard password.
async fn auth_password(
    State(state): State<Arc<AppState>>,
    tls: Tls,
    Json(req): Json<PasswordRequest>,
) -> AppResult {
    if state.auth.password_set() {
        if let Some(secs) = state.auth.locked_for() {
            return Err(fail(
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many wrong passwords; try again in {secs} seconds"),
            ));
        }
        if !verify(&state, req.current).await {
            state.auth.note_failure();
            return Err(fail(StatusCode::FORBIDDEN, "the current password is wrong"));
        }
        state.auth.clear_failures();
    }
    if req.new.is_empty() {
        if !netcfg::NetSettings::load(&state.db).await.address.is_empty() {
            return Err(fail(
                StatusCode::CONFLICT,
                "other computers can reach this dashboard, so it needs a password. Switch network access off under Settings > Network first",
            ));
        }
        state.db.set_setting(auth::PASSWORD_KEY, None).await.map_err(internal)?;
        state.auth.set_hash(None);
        state.activity("warn", "auth", None, "Dashboard password removed");
        return Ok(with_cookie(ok(json!({"password_set": false})), session_cookie("", 0, tls.is_some())));
    }
    auth::validate_new_password(&req.new).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let new = req.new;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&new))
        .await
        .map_err(|e| internal(e.to_string()))?;
    state
        .db
        .set_setting(auth::PASSWORD_KEY, Some(hash.clone()))
        .await
        .map_err(internal)?;
    // Everyone else is signed out; the caller gets a fresh session.
    state.auth.set_hash(Some(hash));
    let token = state.auth.create_session();
    state.activity("info", "auth", None, "Dashboard password set");
    Ok(with_cookie(
        ok(json!({"password_set": true})),
        session_cookie(&token, auth::SESSION_TTL.as_secs(), tls.is_some()),
    ))
}

/// Sandbox guard (PRD section 5). Because this server can execute scripts,
/// every request must be addressed to localhost / a private IP, and a browser
/// request must originate from the dashboard itself. This blocks DNS
/// rebinding and cross-site requests from pages on the public internet.
///
/// When a dashboard password is set, everything except the public paths
/// also needs a signed-in session (or the local CLI's control token).
async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let named = state.net.get().is_some_and(|n| n.host_allowed(&host));
    if !net::host_header_allowed(&host) && !named {
        return fail(StatusCode::FORBIDDEN, "forbidden host");
    }
    if let Some(origin) = req.headers().get(ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if !net::origin_allowed(origin, &host) {
            return fail(StatusCode::FORBIDDEN, "cross-origin requests are not allowed");
        }
    }
    let path = req.uri().path();
    if state.auth.password_set() && !is_public_path(path) && !is_authenticated(&state, req.headers()) {
        return if path == "/" {
            html_page(LOGIN_HTML)
        } else {
            fail(StatusCode::UNAUTHORIZED, "sign in required")
        };
    }
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

async fn index() -> Response {
    html_page(INDEX_HTML)
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
    v["python"] = pyenv::describe(std::path::Path::new(&t.working_directory), t.python_env.as_deref());
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

#[derive(serde::Deserialize)]
struct RollbackRequest {
    sha: String,
}

/// Check out an earlier commit of the tracked branch and deploy it.
async fn rollback_target(
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
async fn project_start(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> AppResult {
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

async fn project_stop(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> AppResult {
    for t in automation::project_members(&state, &name).await.map_err(internal)? {
        reset_restart_state(&state, t.id);
    }
    let stopped = automation::stop_project(&state, &name).await.map_err(internal)?;
    Ok(ok(json!({"stopping": stopped})))
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

/// The processes in a running target's tree, plus the disk its folder is on.
/// Who can reach a target's own port: what its start script was written
/// for, and what really answers right now on each address of this machine.
async fn target_network(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let port = t.health_url.as_deref().and_then(|u| health::parse_url(u).ok()).map(|u| u.port);
    let view = tokio::task::spawn_blocking(move || {
        let dir = std::path::PathBuf::from(&t.working_directory);
        let script = std::fs::read_to_string(dir.join(&t.script_name)).unwrap_or_default();
        let ours = script.contains(GENERATED_MARKER);
        // Only a script PortalControl wrote can be rewritten from the dashboard.
        let starter = ours
            .then(|| detect_project(&dir).starters.into_iter().find(|s| s.script().eq_ignore_ascii_case(&t.script_name)))
            .flatten();
        let answers = |ip: &str| {
            port.and_then(|p| format!("{ip}:{p}").parse::<std::net::SocketAddr>().ok())
                .is_some_and(|addr| std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok())
        };
        let network: Vec<Value> = netcfg::local_addresses()
            .into_iter()
            .map(|a| {
                json!({
                    "ip": a.ip, "interface": a.interface, "kind": a.kind,
                    "url": port.map(|p| format!("http://{}:{p}/", a.ip)),
                    "open": answers(&a.ip),
                })
            })
            .collect();
        json!({
            "port": port,
            "script": t.script_name,
            "script_is_ours": ours,
            "starter": starter.as_ref().map(|s| json!({"id": s.id, "label": s.label})),
            "written_for_network": ours && script.contains("0.0.0.0"),
            "local": {"url": port.map(|p| format!("http://127.0.0.1:{p}/")), "open": answers("127.0.0.1")},
            "network": network,
        })
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    Ok(ok(view))
}

async fn target_processes(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let body = {
        let m = state.metrics.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "processes": m.processes.get(&id).cloned().unwrap_or_default(),
            "disk": m.disk_for(&t.working_directory),
        })
    };
    Ok(ok(body))
}

/// Minute history from the database for the 24-hour and 7-day views:
/// bucketed series, availability, latency figures and the outage list.
async fn metrics_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> AppResult {
    load_target(&state, id).await?;
    let (span, bucket): (i64, i64) = match q.get("range").map(String::as_str) {
        Some("7d") => (7 * 24 * 3600, 1800),
        Some("1h") => (3600, 60),
        _ => (24 * 3600, 300),
    };
    let now = crate::util::now_secs() as i64;
    let rows = state.db.metric_points_since(id, now - span).await.map_err(internal)?;

    let probes: i64 = rows.iter().map(|p| p.probes).sum();
    let up: i64 = rows.iter().map(|p| p.probes_up).sum();
    let mut latencies: Vec<f64> = rows.iter().filter_map(|p| p.latency).collect();
    latencies.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| -> Option<f64> {
        if latencies.is_empty() {
            None
        } else {
            let i = ((latencies.len() as f64 - 1.0) * p).round() as usize;
            Some(latencies[i.min(latencies.len() - 1)])
        }
    };
    let cpu: Vec<f64> = rows.iter().filter_map(|p| p.cpu).collect();
    let running_minutes = cpu.len();
    Ok(ok(json!({
        "range_secs": span,
        "bucket_secs": bucket,
        "points": crate::metrics::bucketed(&rows, bucket),
        "minutes_recorded": rows.len(),
        "running_minutes": running_minutes,
        "availability_pct": if probes > 0 { Some((up as f64 * 10000.0 / probes as f64).round() / 100.0) } else { None },
        "latency_avg": if latencies.is_empty() { None } else { Some(latencies.iter().sum::<f64>() / latencies.len() as f64) },
        "latency_p50": pct(0.5),
        "latency_p95": pct(0.95),
        "latency_max": latencies.last(),
        "cpu_avg": if cpu.is_empty() { None } else { Some(cpu.iter().sum::<f64>() / cpu.len() as f64) },
        "cpu_peak": cpu.iter().cloned().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v)))),
        "mem_peak": rows.iter().filter_map(|p| p.mem_max).max(),
        "outages": crate::metrics::outages(&rows).into_iter().take(25).collect::<Vec<_>>(),
    })))
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

/// A quick command resolved for one project: which program runs it and with
/// which arguments.
#[derive(Debug, Clone)]
struct ResolvedCommand {
    id: String,
    label: String,
    description: String,
    program: String,
    args: Vec<String>,
    changes: bool,
    backup_first: bool,
}

impl ResolvedCommand {
    fn shown(&self) -> String {
        let program = self.program.strip_suffix(".cmd").unwrap_or(&self.program);
        format!("{program} {}", self.args.join(" "))
    }
}

fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

/// Names of the scripts defined in `package.json`, limited to plain names.
fn package_scripts(dir: &std::path::Path) -> Vec<String> {
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
const NPM_SCRIPTS: &[(&str, &str, &str, bool)] = &[
    ("build", "Build", "Run the project's build script.", true),
    ("lint", "Lint", "Run the project's lint script.", false),
    ("test", "Test", "Run the project's test script.", false),
    ("typecheck", "Type check", "Run the project's typecheck script.", false),
    ("migrate", "Run migrations", "Run the project's migrate script.", true),
];

fn detect_commands(dir: &std::path::Path, python_env: Option<&str>) -> Vec<ResolvedCommand> {
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

async fn list_commands(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
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

async fn run_command(
    State(state): State<Arc<AppState>>,
    Path((id, command)): Path<(i64, String)>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = std::path::PathBuf::from(&t.working_directory);
    let Some(cmd) = detect_commands(&dir, t.python_env.as_deref()).into_iter().find(|c| c.id == command) else {
        return Err(fail(StatusCode::NOT_FOUND, "this command is not available for the target"));
    };
    if cmd.backup_first && t.backup_on_deploy {
        backup::before_run(&state, &t).await;
    }
    run_response(runner::start_command(&state, &t, &cmd.label, &cmd.program, &cmd.args).await)
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

// ------------------------------------------------ project type / starters --

/// Every generated script carries this, so PortalControl can tell its own
/// launchers (safe to regenerate) from files the user wrote (never touched).
const GENERATED_MARKER: &str = "Created by PortalControl";

/// A way to run a project that has no script of its own. PortalControl can
/// write the launcher script for it. `{host}` and `{port}` are filled in.
#[derive(Debug, Clone)]
struct Starter {
    id: &'static str,
    label: String,
    description: String,
    lines: Vec<String>,
    default_port: Option<u16>,
    run_mode: &'static str,
    /// Pass the port as the `PORT` environment variable instead of an argument.
    port_env: bool,
}

impl Starter {
    fn script(&self) -> String {
        let ext = if cfg!(windows) { "bat" } else { "sh" };
        format!("portalcontrol_{}.{ext}", self.id.replace('-', "_"))
    }

    fn body(&self, host: &str, port: Option<u16>) -> String {
        let port_text = port.map(|p| p.to_string()).unwrap_or_default();
        let lines: Vec<String> = self
            .lines
            .iter()
            .map(|l| l.replace("{host}", host).replace("{port}", &port_text))
            .collect();
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

struct ProjectInfo {
    kind: &'static str,
    label: &'static str,
    starters: Vec<Starter>,
}

/// Lower-cased text of the Python dependency manifests at the top level.
fn python_deps(dir: &std::path::Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(2_000) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if (name.starts_with("requirements") && name.ends_with(".txt")) || name == "pyproject.toml" || name == "pipfile" {
                text.push_str(&read_small(&entry.path()));
                text.push('\n');
            }
        }
    }
    text
}

/// First immediate sub-directory that contains `file`, e.g. the Django
/// project package (the one holding `wsgi.py`).
fn package_with(dir: &std::path::Path, file: &str) -> Option<String> {
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
fn detect_project(dir: &std::path::Path) -> ProjectInfo {
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
    } else if deps.contains("fastapi") && (has("main.py") || has("app.py") || dir.join("app").join("main.py").is_file()) {
        (kind, label) = ("fastapi", "FastAPI");
        let module = if has("main.py") { "main" } else if has("app.py") { "app" } else { "app.main" };
        starters.push(service(
            "uvicorn",
            "Uvicorn server",
            "Serves the FastAPI app with Uvicorn.",
            format!("{python} -m uvicorn {module}:app --host {{host}} --port {{port}}"),
            8000,
        ));
    } else if deps.contains("streamlit") && (has("app.py") || has("main.py") || has("streamlit_app.py")) {
        (kind, label) = ("streamlit", "Streamlit");
        let file = if has("streamlit_app.py") { "streamlit_app.py" } else if has("app.py") { "app.py" } else { "main.py" };
        starters.push(service(
            "streamlit",
            "Streamlit app",
            "Runs the Streamlit app without opening a browser.",
            format!("{python} -m streamlit run {file} --server.port {{port}} --server.address {{host}} --server.headless true"),
            8501,
        ));
    } else if deps.contains("flask") && (has("app.py") || has("wsgi.py") || has("application.py")) {
        (kind, label) = ("flask", "Flask");
        let module = if has("app.py") { "app" } else if has("wsgi.py") { "wsgi" } else { "application" };
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
        for (script, id, text) in [("start", "npm-start", "npm start"), ("dev", "npm-dev", "npm dev server")] {
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
            "python -m http.server {port} --bind {host}".to_string(),
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
fn create_starter(
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
struct StarterRequest {
    dir: String,
    starter: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

async fn fs_starter(State(state): State<Arc<AppState>>, Json(req): Json<StarterRequest>) -> AppResult {
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
fn port_in_use(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250)).is_ok()
        || std::net::TcpListener::bind(addr).is_err()
}

/// Check a port before it goes into a start script: whether something is
/// listening on it now, whether another target already claims it, and the
/// next port that is clear of both.
/// Which Python environment a setting leads to, before it is saved.
async fn fs_pyenv(Query(q): Params) -> AppResult {
    let dir = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    if dir.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "dir is required"));
    }
    let setting = q.get("env").cloned();
    let shown = tokio::task::spawn_blocking(move || pyenv::describe(std::path::Path::new(&dir), setting.as_deref()))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(shown))
}

async fn fs_port(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
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
async fn fs_list(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let wanted = q.get("dir").map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    // The drives the metrics worker already knows about. Probing every drive
    // letter here would stall for seconds on disconnected network drives.
    let mut roots: Vec<String> = state
        .metrics
        .lock()
        .map(|m| m.disks.iter().map(|d| d.mount.clone()).collect())
        .unwrap_or_default();
    if roots.is_empty() {
        roots.push(if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() });
    }
    let report = tokio::task::spawn_blocking(move || list_dir(&wanted, roots))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(report))
}

fn list_dir(wanted: &str, roots: Vec<String>) -> Value {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from);
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
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
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

/// Long-running npm scripts that are worth supervising as a process.
const NPM_PROCESSES: &[(&str, &str, &str)] = &[
    ("start", "npm_start", "npm start"),
    ("dev", "npm_dev", "npm dev server"),
    ("worker", "npm_worker", "npm worker"),
];

/// Every companion process PortalControl recognises in a project folder.
fn detect_workers(dir: &std::path::Path) -> Vec<Worker> {
    let mut out = detect_django_workers(dir);
    let ext = if cfg!(windows) { "bat" } else { "sh" };
    let defined = package_scripts(dir);
    for (script, kind, label) in NPM_PROCESSES {
        if defined.iter().any(|s| s == script) {
            out.push(Worker {
                kind: *kind,
                label: *label,
                command: format!("npm run {script}"),
                script: format!("portalcontrol_{kind}.{ext}"),
                why: format!("\"{script}\" script in package.json"),
            });
        }
    }
    out
}

/// Look for task-queue workers that belong to a Django project: Django-Q
/// (`manage.py qcluster`) and Celery (worker, and beat when it is configured).
fn detect_django_workers(dir: &std::path::Path) -> Vec<Worker> {
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
            "disks": m.disks,
            "latest": m.host.back(),
        })
    };
    let now = chrono::Local::now();
    Ok(ok(json!({
        "name": "PortalControl-Core",
        "version": env!("CARGO_PKG_VERSION"),
        "author": crate::AUTHOR,
        "homepage": crate::HOMEPAGE,
        "tagline": crate::TAGLINE,
        "commit": option_env!("PORTALCONTROL_COMMIT"),
        "built_at": option_env!("PORTALCONTROL_BUILT").and_then(|s| s.parse::<u64>().ok()),
        "sqlite": rusqlite::version(),
        "libgit2": git::library_version(),
        "started_at": chrono::Utc::now().timestamp() - state.started.elapsed().as_secs() as i64,
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

/// Stop, then start again with the same options. This is how changed
/// network settings are applied.
async fn restart(State(state): State<Arc<AppState>>) -> AppResult {
    state.restart.store(true, Ordering::SeqCst);
    state.activity("warn", "system", None, "PortalControl is restarting");
    state.shutdown.notify_one();
    Ok(ok(json!({"restarting": true})))
}

// ---------------------------------------------------------------- network --

/// Everything the Network settings show: what is stored, what is running,
/// whether the two differ, and what this machine could offer.
async fn network_view(state: &Arc<AppState>) -> Value {
    let saved = netcfg::NetSettings::load(&state.db).await;
    let active = state
        .net
        .get()
        .cloned()
        .unwrap_or_else(|| netcfg::Active::local_only(state.bind.port()));
    let (cli_bind, cli_port) = state.cli_net.get().copied().unwrap_or((None, None));
    let next = netcfg::effective(&saved, cli_bind, cli_port, crate::DEFAULT_PORT);
    let addresses = tokio::task::spawn_blocking(netcfg::local_addresses).await.unwrap_or_default();
    let computer_name = state.metrics.lock().ok().and_then(|m| m.host_name.clone());
    let password_set = state.auth.password_set();

    let mut warnings: Vec<String> = Vec::new();
    if let Some(e) = &active.error {
        warnings.push(format!("Other computers cannot connect right now, because the network address could not be opened: {e}"));
    }
    if !saved.address.is_empty() && !addresses.iter().any(|a| a.ip == saved.address) {
        warnings.push(format!(
            "This computer does not have the address {} at the moment (is the network or Tailscale connected?). Network access stays off until it does and PortalControl is restarted.",
            saved.address
        ));
    }
    if saved.https && saved.address.is_empty() {
        warnings.push("HTTPS is set up but has nothing to protect until an address for other computers is chosen.".into());
    }
    if next.network.is_some() && !next.https {
        warnings.push("Other computers connect without encryption: the password and everything on screen cross the network in clear text. That is fine on Tailscale or a network you trust; otherwise turn on HTTPS.".into());
    }
    if next.network.is_some() && !password_set {
        warnings.push("No dashboard password is set. Anyone who can reach the address can run the registered scripts.".into());
    }

    json!({
        "saved": saved,
        "active": {
            "port": active.effective.port,
            "local_url": active.local_url(),
            "network_url": active.network_url(),
            "https": active.effective.https && active.network.is_some(),
            "hosts": active.effective.hosts,
            "error": active.error,
        },
        "bind_from_cli": next.bind_from_cli,
        "port_from_cli": next.port_from_cli,
        "restart_needed": next != active.effective,
        "addresses": addresses,
        "computer_name": computer_name,
        "password_set": password_set,
        "default_port": crate::DEFAULT_PORT,
        "warnings": warnings,
    })
}

async fn network_get(State(state): State<Arc<AppState>>) -> AppResult {
    Ok(ok(network_view(&state).await))
}

async fn network_put(State(state): State<Arc<AppState>>, Json(input): Json<netcfg::NetSettings>) -> AppResult {
    // Validation reads the certificate files.
    let settings = tokio::task::spawn_blocking(move || input.validate())
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    if !settings.address.is_empty() && !state.auth.password_set() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "set a dashboard password first (Settings > Access). PortalControl runs scripts, so it is not opened to other computers without one",
        ));
    }
    settings.save(&state.db).await.map_err(internal)?;
    state.activity("info", "system", None, "Network settings changed");
    Ok(ok(network_view(&state).await))
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
    fn project_types_are_detected_with_start_options() {
        let ids = |p: &std::path::Path| -> (String, Vec<&'static str>) {
            let info = detect_project(p);
            (info.kind.to_string(), info.starters.iter().map(|s| s.id).collect())
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(ids(p), ("unknown".to_string(), vec![]));

        std::fs::write(p.join("index.html"), "<h1>hi</h1>").unwrap();
        assert_eq!(ids(p), ("static".to_string(), vec!["static"]));

        std::fs::write(p.join("requirements.txt"), "fastapi\nuvicorn\n").unwrap();
        std::fs::write(p.join("main.py"), "app = FastAPI()").unwrap();
        assert_eq!(ids(p), ("fastapi".to_string(), vec!["uvicorn"]));
        assert!(detect_project(p).starters[0].lines[0].contains("uvicorn main:app --host {host} --port {port}"));

        // Django wins, and offers Waitress because it is in the requirements.
        std::fs::write(p.join("manage.py"), "").unwrap();
        std::fs::write(p.join("requirements.txt"), "Django\nwaitress\n").unwrap();
        std::fs::create_dir(p.join("portal")).unwrap();
        std::fs::write(p.join("portal").join("wsgi.py"), "").unwrap();
        std::fs::write(p.join("portal").join("settings.py"), "STATIC_ROOT = BASE_DIR / 'static'\n").unwrap();
        let (kind, starters) = ids(p);
        assert_eq!(kind, "django");
        assert_eq!(starters, vec!["runserver", "waitress", "deploy"]);
        let info = detect_project(p);
        assert!(info.starters[1].lines[0].contains("portal.wsgi:application"));
        assert_eq!(info.starters[2].lines.len(), 3, "pip install, migrate, collectstatic");
        assert_eq!((info.starters[2].run_mode, info.starters[2].default_port), ("TASK", None));

        // A front end next to Django adds its npm start option.
        std::fs::write(p.join("package.json"), r#"{"scripts":{"start":"node server.js"}}"#).unwrap();
        assert_eq!(ids(p).1, vec!["runserver", "waitress", "deploy", "npm-start"]);
    }

    #[test]
    fn start_scripts_are_generated_and_user_files_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("manage.py"), "").unwrap();

        assert!(create_starter(p, "runserver", "127.0.0.1", None).unwrap_err().contains("port"));
        assert!(create_starter(p, "runserver", "8.8.8.8", Some(8000)).is_err());
        assert!(create_starter(p, "nope", "127.0.0.1", Some(8000)).is_err());

        let (script, health) = create_starter(p, "runserver", "127.0.0.1", Some(8123)).unwrap();
        assert_eq!(health.as_deref(), Some("http://127.0.0.1:8123/"));
        assert!(runner::validate_target_paths(&p.to_string_lossy(), &script).is_ok());
        let body = std::fs::read_to_string(p.join(&script)).unwrap();
        assert!(body.contains("manage.py runserver 127.0.0.1:8123 --noreload") && body.contains(GENERATED_MARKER));
        assert!(!body.contains("{port}") && !body.contains("{host}"));

        // Changing the port regenerates our own script ...
        create_starter(p, "runserver", "0.0.0.0", Some(9000)).unwrap();
        assert!(std::fs::read_to_string(p.join(&script)).unwrap().contains("0.0.0.0:9000"));
        // ... but a file the user wrote under that name is never overwritten.
        std::fs::write(p.join(&script), "my own script").unwrap();
        assert!(create_starter(p, "runserver", "127.0.0.1", Some(8000)).unwrap_err().contains("already exists"));
        assert_eq!(std::fs::read_to_string(p.join(&script)).unwrap(), "my own script");

        // The deploy task has no port and stops at the first failing step.
        let (deploy, health) = create_starter(p, "deploy", "127.0.0.1", None).unwrap();
        assert!(health.is_none());
        let body = std::fs::read_to_string(p.join(deploy)).unwrap();
        assert!(body.contains("migrate --noinput"));

        let v = inspect_dir(p);
        assert_eq!(v["project"]["kind"], "django");
        assert_eq!(v["starters"][0]["script_exists"], true);
        assert_eq!(v["starters"][0]["script_is_ours"], false);
    }

    #[test]
    fn folders_are_listed_and_busy_ports_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir(p.join("beta_portal")).unwrap();
        std::fs::create_dir(p.join("Alpha")).unwrap();
        std::fs::create_dir(p.join(".hidden")).unwrap();
        std::fs::write(p.join("beta_portal").join("manage.py"), "").unwrap();
        std::fs::write(p.join("file.txt"), "").unwrap();
        let roots = || vec!["/".to_string()];
        let v = list_dir(&p.to_string_lossy(), roots());
        let names: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Alpha", "beta_portal"]);
        assert_eq!(v["entries"][1]["project"], true);
        assert_eq!(v["entries"][0]["project"], false);
        assert!(v["parent"].is_string() && !v["roots"].as_array().unwrap().is_empty());
        assert!(list_dir("relative/path", roots())["error"].is_string());
        assert!(list_dir("", roots())["dir"].is_string(), "defaults to the home folder");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use(port));
        drop(listener);
        assert!(!port_in_use(port));
    }

    #[test]
    fn node_projects_get_commands_and_processes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert!(detect_commands(p, None).is_empty());
        std::fs::write(
            p.join("package.json"),
            r#"{"name":"portal-ui","scripts":{"build":"vite build","dev":"vite","lint":"eslint .","evil; rm -rf /":"x"}}"#,
        )
        .unwrap();
        let ids: Vec<String> = detect_commands(p, None).iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, vec!["npm-install", "npm-run-build", "npm-run-lint"]);
        std::fs::write(p.join("package-lock.json"), "{}").unwrap();
        let cmds = detect_commands(p, None);
        assert_eq!(cmds[1].id, "npm-ci");
        assert_eq!(cmds[2].shown(), "npm run build");
        assert!(cmds[2].changes && !cmds[3].changes);
        assert_eq!(package_scripts(p), vec!["build", "dev", "lint"]);

        let workers = detect_workers(p);
        assert_eq!(workers.len(), 1);
        assert_eq!((workers[0].kind, workers[0].command.as_str()), ("npm_dev", "npm run dev"));
        let (script, created) = create_launcher(p, "npm_dev").unwrap();
        assert!(created && script.starts_with("portalcontrol_npm_dev."));
        assert!(std::fs::read_to_string(p.join(script)).unwrap().contains("npm run dev"));

        // A Django project with a front end gets both sets.
        std::fs::write(p.join("manage.py"), "").unwrap();
        let ids: Vec<String> = detect_commands(p, None).iter().map(|c| c.id.clone()).collect();
        assert!(ids.contains(&"migrate".to_string()) && ids.contains(&"npm-run-build".to_string()));
    }

    #[test]
    fn public_paths_and_session_cookies() {
        for open in ["/assets/styles.css", "/assets/js/core/app.js", "/api/auth/status", "/api/auth/login", "/api/webhooks/deploy/abc"] {
            assert!(is_public_path(open), "{open}");
        }
        for closed in ["/", "/api/targets", "/ws", "/api/auth/password", "/api/auth/logout", "/api/system/shutdown", "/api/webhooks"] {
            assert!(!is_public_path(closed), "{closed}");
        }
        let cookie = session_cookie("tok", 60, false);
        assert!(cookie.contains("pc_session=tok") && cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("Secure"));
        // Signed in over HTTPS: the cookie never travels over plain HTTP.
        assert!(session_cookie("tok", 60, true).ends_with("; Secure"));

        let state = AppState::new(crate::db::Db::open_in_memory().unwrap(), "127.0.0.1:0".parse().unwrap());
        let mut headers = HeaderMap::new();
        assert!(!is_authenticated(&state, &headers));
        let token = state.auth.create_session();
        headers.insert(COOKIE, format!("x=1; pc_session={token}").parse().unwrap());
        assert!(is_authenticated(&state, &headers));
        state.auth.end_session(&token);
        assert!(!is_authenticated(&state, &headers));
        headers.insert(auth::CONTROL_HEADER, state.auth.control_token.parse().unwrap());
        assert!(is_authenticated(&state, &headers));

        // Every asset the page shell references is embedded.
        for html in [INDEX_HTML, LOGIN_HTML] {
            for part in html.split("\"/assets/").skip(1) {
                let name = part.split('"').next().unwrap();
                assert!(ASSETS.iter().any(|(n, _, _)| *n == name), "{name} is not embedded");
            }
        }
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
