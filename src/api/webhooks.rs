//! Deploy webhooks: URL tokens, signing secrets and the inbound endpoint (the checks
//! themselves are in `crate::webhook`).

use super::*;

pub(super) async fn webhook_enable(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let token = automation::new_token();
    state.db.set_webhook_token(id, Some(token.clone())).await.map_err(internal)?;
    state.activity("info", "target", Some(id), "Deploy webhook URL generated");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_token": token})))
}

pub(super) async fn webhook_disable(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    state.db.set_webhook_token(id, None).await.map_err(internal)?;
    state.db.set_webhook_secret(id, None).await.map_err(internal)?;
    state.activity("info", "target", Some(id), "Deploy webhook disabled");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_token": null})))
}

/// Trigger a deploy from outside the dashboard (a Git server hook, a script,
/// `curl`). The unguessable token in the URL is the credential.
/// A deploy changes code and runs scripts, so it is never triggered by GET:
/// link previews in chat apps, browser prefetching and URL scanners all fetch
/// pasted links, and any of them could otherwise start a deployment.
pub(super) async fn webhook_get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(axum::http::header::ALLOW, "POST")],
        Json(json!({"error": "deploy webhooks only accept POST, e.g. curl -X POST <url>"})),
    )
        .into_response()
}

/// Create (or replace) the signing secret of a target's webhook.
pub(super) async fn webhook_secret_create(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    if t.webhook_token.is_none() {
        return Err(fail(StatusCode::BAD_REQUEST, "generate the webhook URL first"));
    }
    let secret = automation::new_token();
    state.db.set_webhook_secret(id, Some(secret.clone())).await.map_err(internal)?;
    state.activity("info", "target", Some(id), "Webhook signing secret set");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_secret": secret})))
}

pub(super) async fn webhook_secret_remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult {
    load_target(&state, id).await?;
    state.db.set_webhook_secret(id, None).await.map_err(internal)?;
    state.activity("warn", "target", Some(id), "Webhook signing secret removed");
    state.emit(json!({"type": "targets_changed"}));
    Ok(ok(json!({"webhook_secret": null})))
}

pub(super) async fn webhook_deploy(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    Query(q): Params,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> AppResult {
    if token.len() < 16 {
        return Err(fail(StatusCode::NOT_FOUND, "unknown webhook"));
    }
    let Some(t) = state.db.target_by_webhook(token).await.map_err(internal)? else {
        return Err(fail(StatusCode::NOT_FOUND, "unknown webhook"));
    };
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let delivery = webhook::Delivery {
        hub_signature_256: header("x-hub-signature-256"),
        gitea_signature: header("x-gitea-signature"),
        gitlab_token: header("x-gitlab-token"),
        plain_secret: header("x-webhook-secret"),
        github_event: header("x-github-event"),
        body: &body,
    };
    match webhook::check(&delivery, t.webhook_secret.as_deref(), &t.git_branch) {
        webhook::Verdict::Deploy => {}
        webhook::Verdict::Skip(why) => {
            state.activity("info", "deploy", Some(t.id), format!("Webhook received, not deployed: {why}"));
            return Ok(ok(json!({"deployed": false, "reason": why})));
        }
        webhook::Verdict::Reject(why) => {
            state.activity("warn", "deploy", Some(t.id), format!("Webhook refused: {why}"));
            return Err(fail(StatusCode::UNAUTHORIZED, why));
        }
    }
    let pull = q.get("pull").map(|v| v != "0" && v != "false").unwrap_or(true);
    let result = automation::deploy(&state, &t, "WEBHOOK", pull).await;
    if let Err(e) = &result {
        state.activity("error", "deploy", Some(t.id), format!("Webhook deploy rejected: {e}"));
    }
    run_response(result)
}
