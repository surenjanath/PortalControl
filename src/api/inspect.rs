//! Per-target details: security scans, Git state, health probes, metrics, processes
//! and network reachability.

use super::*;

pub(super) async fn start_scan(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match security::start_scan(&state, t) {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(json!({"scanning": true}))).into_response()),
        Err(e) if e.contains("already running") => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

pub(super) async fn latest_scan(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let scan = state.db.latest_scan(id, true).await.map_err(internal)?;
    Ok(ok(json!(scan)))
}

pub(super) async fn list_scans(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let scans = state.db.list_scans(id, 30).await.map_err(internal)?;
    Ok(ok(json!(scans)))
}

pub(super) async fn git_refresh(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let info = git::refresh_target(&state, id, t.working_directory, t.git_branch, true).await;
    Ok(ok(json!(info)))
}

pub(super) async fn git_pull(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match automation::pull(&state, &t).await {
        Ok(outcome) => Ok(ok(json!(outcome))),
        Err(e) => Err(fail(StatusCode::CONFLICT, e)),
    }
}

pub(super) async fn git_log(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let commits = tokio::task::spawn_blocking(move || {
        git::recent_commits(std::path::Path::new(&t.working_directory), &t.git_branch, 30)
    })
    .await
    .map_err(|e| internal(e.to_string()))?
    .unwrap_or_default();
    Ok(ok(json!(commits)))
}

pub(super) async fn git_changes(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let files = tokio::task::spawn_blocking(move || {
        git::changed_files(std::path::Path::new(&t.working_directory), 200)
    })
    .await
    .map_err(|e| internal(e.to_string()))?
    .unwrap_or_default();
    Ok(ok(json!(files)))
}

pub(super) async fn health_check(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    match t.health_url {
        Some(url) => Ok(ok(json!(health::probe_target(&state, id, &url).await))),
        None => Err(fail(StatusCode::BAD_REQUEST, "target has no health URL")),
    }
}

pub(super) async fn health_history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let samples: Vec<_> = state
        .health_history
        .lock()
        .map(|m| m.get(&id).map(|r| r.iter().cloned().collect()).unwrap_or_default())
        .unwrap_or_default();
    Ok(ok(json!({"samples": samples, "probe_interval_secs": 15})))
}

pub(super) async fn target_metrics(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
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
pub(super) async fn target_network(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let port = t.health_url.as_deref().and_then(|u| health::parse_url(u).ok()).map(|u| u.port);
    let view = tokio::task::spawn_blocking(move || {
        let dir = std::path::PathBuf::from(&t.working_directory);
        let script = std::fs::read_to_string(dir.join(&t.script_name)).unwrap_or_default();
        let ours = script.contains(GENERATED_MARKER);
        // Only a script PortalControl wrote can be rewritten from the dashboard.
        let starter = ours
            .then(|| {
                detect_project(&dir)
                    .starters
                    .into_iter()
                    .find(|s| s.script().eq_ignore_ascii_case(&t.script_name))
            })
            .flatten();
        let answers = |ip: &str| {
            port.and_then(|p| format!("{ip}:{p}").parse::<std::net::SocketAddr>().ok()).is_some_and(|addr| {
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok()
            })
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

pub(super) async fn target_processes(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
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
pub(super) async fn metrics_history(
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
