//! The manager itself: system info, its own log, start at login, shutdown and
//! restart, and its network settings.

use super::*;

pub(super) async fn system(State(state): State<Arc<AppState>>) -> AppResult {
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

pub(super) async fn system_metrics(State(state): State<Arc<AppState>>) -> AppResult {
    let body = {
        let m = state.metrics.lock().unwrap_or_else(|p| p.into_inner());
        let samples: Vec<_> = m.host.iter().cloned().collect();
        json!({"samples": samples, "cpus": m.cpus, "mem_total": m.mem_total, "interval_secs": 5})
    };
    Ok(ok(body))
}

/// Last part of a text file, starting at a line boundary.
pub(super) fn tail_file(path: &std::path::Path, max_bytes: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 { text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or(text) } else { text })
}

pub(super) async fn system_log(State(state): State<Arc<AppState>>) -> AppResult {
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

pub(super) async fn autostart_install(State(state): State<Arc<AppState>>) -> AppResult {
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

pub(super) async fn autostart_remove(State(state): State<Arc<AppState>>) -> AppResult {
    let result =
        tokio::task::spawn_blocking(autostart::uninstall).await.map_err(|e| internal(e.to_string()))?;
    match result {
        Ok(message) => {
            state.activity("info", "system", None, "Start at login disabled");
            Ok(ok(json!({"message": message, "autostart": null})))
        }
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

pub(super) async fn shutdown(State(state): State<Arc<AppState>>) -> AppResult {
    state.shutdown.notify_one();
    Ok(ok(json!({"shutting_down": true})))
}

/// Stop, then start again with the same options. This is how changed
/// network settings are applied.
pub(super) async fn restart(State(state): State<Arc<AppState>>) -> AppResult {
    state.restart.store(true, Ordering::SeqCst);
    state.activity("warn", "system", None, "PortalControl is restarting");
    state.shutdown.notify_one();
    Ok(ok(json!({"restarting": true})))
}

// ---------------------------------------------------------------- network --

/// Everything the Network settings show: what is stored, what is running,
/// whether the two differ, and what this machine could offer.
pub(super) async fn network_view(state: &Arc<AppState>) -> Value {
    let saved = netcfg::NetSettings::load(&state.db).await;
    let active = state.net.get().cloned().unwrap_or_else(|| netcfg::Active::local_only(state.bind.port()));
    let (cli_bind, cli_port) = state.cli_net.get().copied().unwrap_or((None, None));
    let next = netcfg::effective(&saved, cli_bind, cli_port, crate::DEFAULT_PORT);
    let addresses = tokio::task::spawn_blocking(netcfg::local_addresses).await.unwrap_or_default();
    let computer_name = state.metrics.lock().ok().and_then(|m| m.host_name.clone());
    let password_set = state.auth.password_set();

    let mut clients: Vec<Value> = state
        .clients
        .lock()
        .map(|seen| {
            seen.iter()
                .map(|(ip, c)| json!({"ip": ip.to_string(), "last_seen": c.last_seen, "requests": c.requests, "refused": c.refused}))
                .collect()
        })
        .unwrap_or_default();
    clients.sort_by_key(|c| std::cmp::Reverse(c["last_seen"].as_i64().unwrap_or(0)));

    let mut warnings: Vec<String> = Vec::new();
    if let Some(e) = &active.error {
        warnings.push(format!(
            "Other computers cannot connect right now, because the network address could not be opened: {e}"
        ));
    }
    if !saved.address.is_empty() && !addresses.iter().any(|a| a.ip == saved.address) {
        warnings.push(format!(
            "This computer does not have the address {} at the moment (is the network or Tailscale connected?). Network access stays off until it does and PortalControl is restarted.",
            saved.address
        ));
    }
    if saved.https && saved.address.is_empty() {
        warnings.push(
            "HTTPS is set up but has nothing to protect until an address for other computers is chosen."
                .into(),
        );
    }
    if next.network.is_some() && !next.https {
        warnings.push("Other computers connect without encryption: the password and everything on screen cross the network in clear text. That is fine on Tailscale or a network you trust; otherwise turn on HTTPS.".into());
    }
    if let Some(ip) = next.network.filter(|ip| !net::is_private_ip(ip)) {
        warnings.push(format!(
            "{ip} is outside the address ranges reserved for private networks. You confirmed your network uses it internally; make sure a firewall keeps this computer unreachable from the internet."
        ));
    }
    if next.network.is_some() && !password_set {
        warnings.push(
            "No dashboard password is set. Anyone who can reach the address can run the registered scripts."
                .into(),
        );
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
        "limits": {
            "allow_other_networks": saved.allow_other_networks,
            "expires_at": saved.expires_at,
            "expired": saved.expires_at.is_some_and(|end| chrono::Utc::now().timestamp() >= end),
        },
        "subnet": active.subnet_text(),
        "clients": clients,
        "warnings": warnings,
    })
}

pub(super) async fn network_get(State(state): State<Arc<AppState>>) -> AppResult {
    Ok(ok(network_view(&state).await))
}

pub(super) async fn network_put(
    State(state): State<Arc<AppState>>,
    Json(input): Json<netcfg::NetSettings>,
) -> AppResult {
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
    // The limits on other computers apply at once.
    *state.net_limits.lock().unwrap_or_else(|p| p.into_inner()) = netcfg::Limits::from(&settings);
    state.activity("info", "system", None, "Network settings changed");
    Ok(ok(network_view(&state).await))
}
