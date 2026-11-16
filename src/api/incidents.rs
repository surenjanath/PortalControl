//! Incidents from the monitor, applying a suggested fix, and the settings of
//! the monitor (local AI model) and of email alerts.

use super::*;
use crate::{alerts, monitor, ollama};

/// The quick commands of a target, as (id, label), for the monitor to offer.
pub(crate) fn command_choices(t: &Target) -> Vec<(String, String)> {
    detect_commands(std::path::Path::new(&t.working_directory), t.python_env.as_deref())
        .into_iter()
        .map(|c| (c.id, c.label))
        .collect()
}

pub(super) async fn incidents_list(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let open_only = q.get("status").map(|s| s == "open").unwrap_or(false);
    let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 500);
    let list = state.db.list_incidents(open_only, limit).await.map_err(internal)?;
    Ok(ok(json!(list)))
}

#[derive(serde::Deserialize)]
pub(super) struct StatusRequest {
    pub(super) status: String,
}

pub(super) async fn incident_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<StatusRequest>,
) -> AppResult {
    let status: &'static str = match req.status.as_str() {
        "resolved" => "resolved",
        "dismissed" => "dismissed",
        "open" => "open",
        _ => return Err(fail(StatusCode::BAD_REQUEST, "status must be open, resolved or dismissed")),
    };
    if !state.db.set_incident_status(id, status, String::new()).await.map_err(internal)? {
        return Err(fail(StatusCode::NOT_FOUND, "no such incident"));
    }
    state.emit(json!({"type": "incident", "id": id}));
    Ok(ok(json!({"id": id, "status": status})))
}

/// Carry out the incident's suggested action, now that a person asked for
/// it. Only the action stored with the incident can run, and only if it is
/// still on offer for the target.
pub(super) async fn incident_act(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let inc = state
        .db
        .get_incident(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such incident"))?;
    let tid = inc.target_id.ok_or_else(|| fail(StatusCode::BAD_REQUEST, "this incident has no target"))?;
    let t = load_target(&state, tid).await?;
    let offered = monitor::choices(&state, &t).await;
    if inc.action == "none" || !offered.iter().any(|c| c.id == inc.action) {
        return Err(fail(
            StatusCode::CONFLICT,
            "this fix is no longer available for the target; open it and act by hand",
        ));
    }
    let started = if inc.action == "restart" {
        runner::restart(&state, &t, "MANUAL").await
    } else if inc.action == "deploy" {
        automation::deploy(&state, &t, "MANUAL", true).await
    } else if let Some(sha) = inc.action.strip_prefix("rollback:") {
        automation::rollback(&state, &t, sha).await
    } else if let Some(cmd_id) = inc.action.strip_prefix("command:") {
        let dir = std::path::PathBuf::from(&t.working_directory);
        let cmd = detect_commands(&dir, t.python_env.as_deref())
            .into_iter()
            .find(|c| c.id == cmd_id)
            .ok_or_else(|| fail(StatusCode::CONFLICT, "that command is not available any more"))?;
        if cmd.backup_first && t.backup_on_deploy {
            backup::before_run(&state, &t).await;
        }
        runner::start_command(&state, &t, &cmd.label, &cmd.program, &cmd.args).await
    } else {
        return Err(fail(StatusCode::BAD_REQUEST, "unknown action"));
    };
    let run_id = started.map_err(|e| fail(StatusCode::CONFLICT, e))?;
    let note = format!("Applied: {} (run #{run_id})", inc.action_label);
    state.db.set_incident_status(id, "resolved", note.clone()).await.map_err(internal)?;
    if !inc.trace_id.is_empty() {
        let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = state.db.trace_note(inc.trace_id.clone(), "applied", format!("{note} at {at}")).await;
    }
    state.activity("info", "ai", Some(t.id), format!("Incident #{id}: {note}"));
    state.emit(json!({"type": "incident", "id": id}));
    Ok((StatusCode::ACCEPTED, Json(json!({"run_id": run_id, "note": note}))).into_response())
}

// ---------------------------------------------------------------- settings --

pub(super) async fn monitor_get(State(state): State<Arc<AppState>>) -> AppResult {
    Ok(ok(json!(monitor::load(&state).await)))
}

pub(super) async fn monitor_put(
    State(state): State<Arc<AppState>>,
    Json(s): Json<monitor::Settings>,
) -> AppResult {
    monitor::save(&state, &s).await.map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity(
        "info",
        "ai",
        None,
        format!(
            "Monitor settings saved: {}{}",
            if s.enabled { "on" } else { "off" },
            if s.ai_enabled { format!(", AI analysis with {}", s.model) } else { ", no AI analysis".into() }
        ),
    );
    Ok(ok(json!(s)))
}

pub(super) async fn ai_models(Query(q): Params) -> AppResult {
    let url = q.get("url").cloned().unwrap_or_else(|| ollama::DEFAULT_URL.to_string());
    let models = ollama::models(&url).await.map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    Ok(ok(json!({"models": models})))
}

#[derive(serde::Deserialize)]
pub(super) struct AiTestRequest {
    pub(super) url: String,
    pub(super) model: String,
}

/// Run the model on a made-up incident, to see that it answers and how fast.
pub(super) async fn ai_test(Json(req): Json<AiTestRequest>) -> AppResult {
    let t = Target {
        name: "shop-web".into(),
        project: "shop".into(),
        run_mode: "SERVICE".into(),
        working_directory: "/srv/shop".into(),
        script_name: "start.sh".into(),
        ..Default::default()
    };
    let actions = vec![
        monitor::Choice { id: "restart".into(), label: "Restart shop-web".into() },
        monitor::Choice { id: "command:migrate".into(), label: "Run \"Apply migrations\"".into() },
    ];
    let trigger = monitor::Trigger::new(
        0,
        monitor::TriggerKind::ErrorBurst,
        "14 error lines in the last minute (usually about 0.2 per minute)".into(),
    );
    let lines: Vec<String> = [
        "[2026-11-12 10:41:07] INFO GET /products 200 41ms",
        "[2026-11-12 10:41:09] ERROR Internal Server Error: /checkout",
        "Traceback (most recent call last):",
        "  File \"/srv/shop/shop/views.py\", line 88, in checkout",
        "    order = Order.objects.create(customer=request.user, status=\"new\")",
        "django.db.utils.OperationalError: no such column: shop_order.status",
        "[2026-11-12 10:41:12] ERROR Internal Server Error: /checkout",
        "django.db.utils.OperationalError: no such column: shop_order.status",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let user = monitor::prompt(&t, &trigger, "Now: running; health check: LIVE", &lines, &actions);
    let (answer, usage) = ollama::chat_json(
        &req.url,
        &req.model,
        monitor::SYSTEM_PROMPT,
        &user,
        &monitor::answer_schema(&actions),
        std::time::Duration::from_secs(180),
    )
    .await
    .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    Ok(ok(json!({
        "answer": answer,
        "seconds": usage.took.as_secs_f64(),
        "prompt_tokens": usage.prompt_tokens,
        "answer_tokens": usage.answer_tokens,
    })))
}

pub(super) async fn alerts_get(State(state): State<Arc<AppState>>) -> AppResult {
    Ok(ok(alerts::public_view(&alerts::load(&state).await)))
}

pub(super) async fn alerts_put(
    State(state): State<Arc<AppState>>,
    Json(s): Json<alerts::Settings>,
) -> AppResult {
    alerts::save(&state, s).await.map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity("info", "alert", None, "Email alert settings saved");
    Ok(ok(alerts::public_view(&alerts::load(&state).await)))
}

/// Send a test email with the settings in the form (an empty password uses the saved one).
pub(super) async fn alerts_test(
    State(state): State<Arc<AppState>>,
    Json(mut s): Json<alerts::Settings>,
) -> AppResult {
    if s.smtp.password.is_empty() {
        s.smtp.password = alerts::load(&state).await.smtp.password;
    }
    s.smtp.to.retain(|t| !t.trim().is_empty());
    let host =
        state.metrics.lock().ok().and_then(|m| m.host_name.clone()).unwrap_or_else(|| "this computer".into());
    alerts::send_test(&s, &host).await.map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    Ok(ok(json!({"sent": true, "to": s.smtp.to})))
}

/// One trace: the incident's whole chain, from log lines to fix.
pub(super) async fn trace_get(State(state): State<Arc<AppState>>, Path(trace_id): Path<String>) -> AppResult {
    let trace = state
        .db
        .get_trace(trace_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such trace"))?;
    let incident = match trace.incident_id {
        Some(id) => state.db.get_incident(id).await.map_err(internal)?,
        None => None,
    };
    let target = match trace.target_id {
        Some(id) => state
            .db
            .get_target(id)
            .await
            .map_err(internal)?
            .map(|t| json!({"id": t.id, "name": t.name, "project": t.project})),
        None => None,
    };
    Ok(ok(json!({"trace": trace, "incident": incident, "target": target})))
}

/// The request-log middleware for Django projects (`threat.rs`), as a file
/// to save next to `manage.py`.
pub(super) async fn django_middleware() -> axum::response::Response {
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
    (
        [
            (CONTENT_TYPE, "text/x-python; charset=utf-8"),
            (CONTENT_DISPOSITION, "attachment; filename=\"portalcontrol_requestlog.py\""),
        ],
        crate::threat::DJANGO_MIDDLEWARE,
    )
        .into_response()
}

/// Counters and settings for the live pipeline view.
pub(super) async fn monitor_stats(State(state): State<Arc<AppState>>) -> AppResult {
    let settings = monitor::load(&state).await;
    let stats = monitor::stats();
    let targets: serde_json::Map<String, Value> = stats
        .targets
        .iter()
        .map(|(id, (lines, errors, patterns, learning))| {
            (
                id.to_string(),
                json!({"lines": lines, "error_lines": errors, "patterns": patterns, "learning": learning}),
            )
        })
        .collect();
    Ok(ok(json!({
        "enabled": settings.enabled,
        "ai_enabled": settings.ai_enabled,
        "model": settings.model,
        "lines": stats.lines,
        "error_lines": stats.error_lines,
        "flagged": stats.flagged,
        "requests": stats.requests,
        "threats": stats.threats,
        "ai_calls": stats.ai_calls,
        "ai_failed": stats.ai_failed,
        "ai_avg_ms": if stats.ai_calls > stats.ai_failed { Some(stats.ai_ms_total / (stats.ai_calls - stats.ai_failed)) } else { None },
        "last_ai_ms": stats.last_ai_ms,
        "emails": stats.emails,
        "last_trace": stats.last_trace,
        "open_incidents": state.db.open_incident_count().await.unwrap_or(0),
        "targets": targets,
    })))
}
