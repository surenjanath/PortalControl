//! Sign-in, sessions, and the guards every request passes through (allowed hosts,
//! same-origin, the network gate).

use super::*;

pub(super) fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| auth::cookie_value(c, auth::SESSION_COOKIE))
        .map(str::to_string)
}

pub(super) fn is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    if let Some(token) = headers.get(auth::CONTROL_HEADER).and_then(|v| v.to_str().ok()) {
        if state.auth.control_token_matches(token) {
            return true;
        }
    }
    session_token(headers).map(|t| state.auth.session_valid(&t)).unwrap_or(false)
}

/// Paths that must work without a session: static files, the sign-in calls
/// and deploy webhooks (whose URL token is their own credential).
pub(super) fn is_public_path(path: &str) -> bool {
    path.starts_with("/assets/")
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        || path.starts_with("/api/webhooks/deploy/")
}

/// Marks requests that arrived through the HTTPS listener.
#[derive(Clone, Copy)]
pub struct ViaTls;
pub(super) type Tls = Option<axum::Extension<ViaTls>>;

/// `secure`: the request came over HTTPS, so the browser must only ever send
/// the cookie back over HTTPS.
pub(super) fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{}",
        auth::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

pub(super) fn with_cookie(mut res: Response, cookie: String) -> Response {
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    res
}

pub(super) async fn auth_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult {
    let password_set = state.auth.password_set();
    Ok(ok(json!({
        "password_set": password_set,
        "authenticated": !password_set || is_authenticated(&state, &headers),
        "min_password_chars": auth::MIN_PASSWORD_CHARS,
    })))
}

#[derive(serde::Deserialize)]
pub(super) struct LoginRequest {
    pub(super) password: String,
}

pub(super) async fn verify(state: &Arc<AppState>, password: String) -> bool {
    let Some(hash) = state.auth.current_hash() else { return false };
    tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash)).await.unwrap_or(false)
}

pub(super) async fn auth_login(
    State(state): State<Arc<AppState>>,
    tls: Tls,
    client: Option<axum::Extension<ClientIp>>,
    Json(req): Json<LoginRequest>,
) -> AppResult {
    if !state.auth.password_set() {
        return Ok(ok(json!({"authenticated": true})));
    }
    let from = match client {
        Some(axum::Extension(ClientIp(ip))) => format!("from {ip}"),
        None => "on this computer".to_string(),
    };
    let who: auth::Client = client.as_ref().map(|c| c.0 .0);
    if let Some(secs) = state.auth.locked_for(who) {
        return Err(fail(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many wrong passwords; try again in {secs} seconds"),
        ));
    }
    if !verify(&state, req.password).await {
        state.auth.note_failure(who);
        state.activity("warn", "auth", None, format!("Failed sign-in attempt {from}"));
        return Err(fail(StatusCode::UNAUTHORIZED, "wrong password"));
    }
    state.auth.clear_failures(who);
    state.activity("info", "auth", None, format!("Signed in {from}"));
    let token = state.auth.create_session();
    Ok(with_cookie(
        ok(json!({"authenticated": true})),
        session_cookie(&token, auth::SESSION_TTL.as_secs(), tls.is_some()),
    ))
}

pub(super) async fn auth_logout(
    State(state): State<Arc<AppState>>,
    tls: Tls,
    headers: HeaderMap,
) -> AppResult {
    if let Some(token) = session_token(&headers) {
        state.auth.end_session(&token);
    }
    Ok(with_cookie(ok(json!({"authenticated": false})), session_cookie("", 0, tls.is_some())))
}

#[derive(serde::Deserialize)]
pub(super) struct PasswordRequest {
    #[serde(default)]
    pub(super) current: String,
    #[serde(default)]
    pub(super) new: String,
}

/// Set, change or (with an empty new password) remove the dashboard password.
pub(super) async fn auth_password(
    State(state): State<Arc<AppState>>,
    tls: Tls,
    client: Option<axum::Extension<ClientIp>>,
    Json(req): Json<PasswordRequest>,
) -> AppResult {
    if state.auth.password_set() {
        let who: auth::Client = client.as_ref().map(|c| c.0 .0);
        if let Some(secs) = state.auth.locked_for(who) {
            return Err(fail(
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many wrong passwords; try again in {secs} seconds"),
            ));
        }
        if !verify(&state, req.current).await {
            state.auth.note_failure(who);
            return Err(fail(StatusCode::FORBIDDEN, "the current password is wrong"));
        }
        state.auth.clear_failures(who);
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
    state.db.set_setting(auth::PASSWORD_KEY, Some(hash.clone())).await.map_err(internal)?;
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
pub(super) async fn guard(state: State<Arc<AppState>>, req: Request, next: Next) -> Response {
    // Every answer, refusals and the sign-in page included, carries these.
    let mut res = checked(state, req, next).await;
    let headers = res.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(axum::http::header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(axum::http::header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res
}

pub(super) async fn checked(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
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
    next.run(req).await
}

/// The address of the other computer a request came from.
#[derive(Clone, Copy)]
pub struct ClientIp(pub std::net::IpAddr);

/// First check for everything that arrives through the network address:
/// is this computer allowed to talk to PortalControl at all right now?
/// Requests on 127.0.0.1 never pass through here.
pub async fn network_guard(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(remote): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = remote.ip();
    let now = chrono::Utc::now().timestamp();
    let limits = *state.net_limits.lock().unwrap_or_else(|p| p.into_inner());
    let refused = state.net.get().and_then(|n| n.refuse(limits, ip, now, state.auth.password_set()));

    // Remember who came by, and note it when that is news.
    let news = {
        let mut seen = state.clients.lock().unwrap_or_else(|p| p.into_inner());
        if seen.len() >= 200 && !seen.contains_key(&ip) {
            if let Some(oldest) = seen.iter().min_by_key(|(_, c)| c.last_seen).map(|(k, _)| *k) {
                seen.remove(&oldest);
            }
        }
        let entry = seen.entry(ip).or_default();
        let news = entry.requests == 0 || entry.refused != refused;
        entry.last_seen = now;
        entry.requests += 1;
        entry.refused = refused;
        news
    };
    if news {
        match refused {
            Some(why) => state.activity("warn", "network", None, format!("Refused {ip}: {why}")),
            None => state.activity("info", "network", None, format!("{ip} connected")),
        }
    }
    if let Some(why) = refused {
        return (StatusCode::FORBIDDEN, why).into_response();
    }
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

pub(super) async fn index() -> Response {
    html_page(INDEX_HTML)
}
