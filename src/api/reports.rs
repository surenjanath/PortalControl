//! PDF reports (status, site, security; see `crate::report`), the security
//! posture behind the security report (`crate::posture`), and the report
//! branding: organisation details and a logo.

use super::*;
use crate::pdf::Jpeg;
use crate::posture;
use crate::report::{self, Brand};

/// The logo is stored as this file in the data folder.
const LOGO_FILE: &str = "report-logo.jpg";
/// Largest logo accepted. The dashboard shrinks pictures before sending them.
const LOGO_LIMIT: usize = 1024 * 1024;

fn pdf_response(bytes: Vec<u8>, file: &str) -> Response {
    (
        [
            (CONTENT_TYPE, "application/pdf".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\"")),
            (CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// A safe file-name part: letters, digits, `-` and `_`.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "project".into()
    } else {
        s
    }
}

fn logo_path(state: &AppState) -> Option<std::path::PathBuf> {
    state.data_dir.get().map(|d| d.join(LOGO_FILE))
}

async fn load_brand(state: &AppState) -> Brand {
    state
        .db
        .get_setting(report::BRAND_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

async fn load_logo(state: &AppState) -> Option<Jpeg> {
    let path = logo_path(state)?;
    let bytes = tokio::fs::read(path).await.ok()?;
    Jpeg::parse(&bytes).ok()
}

/// Incidents opened in the last `days` days, newest first.
async fn incidents_since(state: &AppState, days: u64) -> Result<Vec<crate::db::Incident>, Response> {
    let since = crate::util::fmt_utc(crate::util::now_secs().saturating_sub(days * 86_400));
    let mut list = state.db.list_incidents(false, 500).await.map_err(internal)?;
    list.retain(|i| i.created_at >= since);
    Ok(list)
}

async fn gather(state: &Arc<AppState>, project: Option<&str>) -> Result<report::Input, Response> {
    let mut targets = state.db.list_targets().await.map_err(internal)?;
    if let Some(p) = project {
        targets.retain(|t| t.project == p);
        if targets.is_empty() {
            return Err(fail(StatusCode::NOT_FOUND, "no project with that name"));
        }
        targets.sort_by_key(|t| (t.start_order, t.name.to_lowercase()));
    } else {
        targets.sort_by_key(|t| (t.project.to_lowercase(), t.start_order, t.name.to_lowercase()));
    }
    let ids: Vec<i64> = targets.iter().map(|t| t.id).collect();
    let mut extras = load_extras(state, &targets).await;
    let mut views = Vec::with_capacity(targets.len());
    for t in targets {
        views.push(target_view(state, t, &mut extras).await);
    }

    let mut recent_runs = state.db.list_recent_runs(500).await.map_err(internal)?;
    recent_runs.retain(|r| ids.contains(&r.target_id));

    let mut runs_by_target = HashMap::new();
    let mut scans = HashMap::new();
    let mut usage_24h = HashMap::new();
    let since = crate::util::now_secs() as i64 - 24 * 3600;
    for &id in &ids {
        if project.is_some() {
            runs_by_target.insert(id, state.db.list_runs(id, 20).await.map_err(internal)?);
        }
        if let Some(scan) = state.db.latest_scan(id, true).await.map_err(internal)? {
            scans.insert(id, scan);
        }
        let points = state.db.metric_points_since(id, since).await.map_err(internal)?;
        let cpus: Vec<f64> = points.iter().filter_map(|p| p.cpu).collect();
        let mems: Vec<i64> = points.iter().filter_map(|p| p.mem).collect();
        let avg_cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
        let avg_mem = (!mems.is_empty()).then(|| mems.iter().sum::<i64>() / mems.len() as i64);
        let peak = points.iter().filter_map(|p| p.mem_max.or(p.mem)).max();
        usage_24h.insert(id, (avg_cpu, avg_mem, peak));
    }

    let now = crate::util::now_secs() as i64;
    let offset = chrono::Local::now().offset().local_minus_utc() as i64;
    let analytics = crate::analytics::build(&state.db, &crate::monitor::stats(), Some("7d"), now, offset)
        .await
        .unwrap_or(Value::Null);
    let mut incidents = incidents_since(state, 7).await?;
    incidents.retain(|i| i.target_id.is_some_and(|id| ids.contains(&id)));

    let host =
        state.metrics.lock().ok().and_then(|m| m.host_name.clone()).unwrap_or_else(|| "this computer".into());
    Ok(report::Input {
        project: project.map(str::to_string),
        host,
        version: env!("CARGO_PKG_VERSION").to_string(),
        generated: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        targets: views,
        recent_runs,
        runs_by_target,
        scans,
        usage_24h,
        brand: load_brand(state).await,
        logo: load_logo(state).await,
        analytics,
        incidents,
        posture: None,
        scan_facts: HashMap::new(),
    })
}

/// The security posture of everything, with the scan facts per target.
async fn assess(
    state: &Arc<AppState>,
    attacks: &[crate::db::Incident],
) -> Result<(posture::Posture, HashMap<i64, posture::ScanFacts>), Response> {
    let targets = state.db.list_targets().await.map_err(internal)?;
    let now = crate::util::now_secs() as i64;
    let mut facts = Vec::with_capacity(targets.len());
    let mut scan_facts = HashMap::new();
    for t in &targets {
        if let Some(scan) = state.db.latest_scan(t.id, true).await.map_err(internal)? {
            let f = posture::scan_facts(&scan.raw_payload.unwrap_or(Value::Null), &scan.scanned_at, now);
            scan_facts.insert(t.id, f);
        }
    }
    for t in targets {
        // Reading the project's .env and the repository index touches the disk.
        let (dir, repo, stored) = (t.working_directory.clone(), git::repo_dir(&t), t.env_vars.clone());
        let (env, tracked) = tokio::task::spawn_blocking(move || {
            let mut env = runner::parse_env(&stored).unwrap_or_default();
            let file = std::path::Path::new(&dir).join(".env");
            if std::fs::metadata(&file).is_ok_and(|m| m.len() <= 64 * 1024) {
                if let Ok(text) = std::fs::read_to_string(&file) {
                    // The stored variables are applied after the file, so they win.
                    let mut all = posture::parse_env_text(&text);
                    all.append(&mut env);
                    env = all;
                }
            }
            (env, git::tracked_env_files(std::path::Path::new(&repo)))
        })
        .await
        .unwrap_or_default();
        let mine: Vec<&crate::db::Incident> = attacks.iter().filter(|i| i.target_id == Some(t.id)).collect();
        let git_dirty =
            state.git.lock().ok().and_then(|g| g.get(&t.id).map(|g| g.status == git::DIRTY)).unwrap_or(false);
        facts.push(posture::TargetFacts {
            id: t.id,
            name: t.name.clone(),
            service: t.run_mode == "SERVICE",
            scan: scan_facts.get(&t.id).cloned(),
            env,
            tracked_env_files: tracked,
            webhook: t.webhook_token.is_some(),
            webhook_secret: t.webhook_secret.is_some(),
            git_dirty,
            attacks_30d: mine.len(),
            attacks_open: mine.iter().filter(|i| i.status == "open").count(),
        });
    }
    let net = state.net();
    let (allow_other, expires) = state
        .net_limits
        .lock()
        .map(|l| (l.allow_other_networks, l.expires_at.is_some()))
        .unwrap_or((false, false));
    let platform = posture::Platform {
        password_set: state.auth.password_set(),
        network_open: net.as_ref().is_some_and(|n| n.network.is_some()),
        network_https: net.as_ref().is_some_and(|n| n.effective.https),
        allow_other_networks: allow_other,
        network_expires: expires,
        monitor_enabled: crate::monitor::load(state).await.enabled,
        alerts_enabled: crate::alerts::load(state).await.enabled,
    };
    Ok((posture::assess(&platform, &facts), scan_facts))
}

/// The attack incidents of the last 30 days.
async fn attacks_30d(state: &AppState) -> Result<Vec<crate::db::Incident>, Response> {
    let mut list = incidents_since(state, 30).await?;
    list.retain(|i| matches!(i.trigger.as_str(), "attack" | "brute_force" | "flood"));
    Ok(list)
}

pub(super) async fn report_overall(State(state): State<Arc<AppState>>) -> AppResult {
    let input = gather(&state, None).await?;
    let date = chrono::Local::now().format("%Y-%m-%d");
    state.activity("info", "report", None, "Status report downloaded");
    Ok(pdf_response(report::overall(&input), &format!("status-report-{date}.pdf")))
}

pub(super) async fn report_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult {
    let input = gather(&state, Some(&name)).await?;
    let date = chrono::Local::now().format("%Y-%m-%d");
    state.activity("info", "report", None, format!("Site report of '{name}' downloaded"));
    Ok(pdf_response(report::project(&input), &format!("site-report-{}-{date}.pdf", slug(&name))))
}

pub(super) async fn report_security(State(state): State<Arc<AppState>>) -> AppResult {
    let mut input = gather(&state, None).await?;
    let attacks = attacks_30d(&state).await?;
    let (posture, scan_facts) = assess(&state, &attacks).await?;
    input.incidents = attacks;
    input.posture = Some(posture);
    input.scan_facts = scan_facts;
    let date = chrono::Local::now().format("%Y-%m-%d");
    state.activity("info", "report", None, "Security assessment report downloaded");
    Ok(pdf_response(report::security(&input), &format!("security-assessment-{date}.pdf")))
}

/// `GET /api/security/posture`: the score, findings and checks for the Security page.
pub(super) async fn security_posture(State(state): State<Arc<AppState>>) -> AppResult {
    let attacks = attacks_30d(&state).await?;
    let (posture, scan_facts) = assess(&state, &attacks).await?;
    Ok(ok(json!({"posture": posture, "scans": scan_facts, "attacks_30d": attacks.len()})))
}

// ---------------------------------------------------------------- branding --

pub(super) async fn brand_get(State(state): State<Arc<AppState>>) -> AppResult {
    let brand = load_brand(&state).await;
    let logo = load_logo(&state).await;
    Ok(ok(json!({
        "brand": brand,
        "logo": logo.map(|l| json!({"width": l.width, "height": l.height})),
    })))
}

pub(super) async fn brand_put(State(state): State<Arc<AppState>>, Json(brand): Json<Brand>) -> AppResult {
    let brand = brand.clean().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let text = serde_json::to_string(&brand).map_err(|e| internal(e.to_string()))?;
    state.db.set_setting(report::BRAND_KEY, Some(text)).await.map_err(internal)?;
    state.activity("info", "report", None, "Report branding saved");
    Ok(ok(json!({"brand": brand})))
}

pub(super) async fn logo_get(State(state): State<Arc<AppState>>) -> AppResult {
    let path = logo_path(&state).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no logo"))?;
    let bytes = tokio::fs::read(path).await.map_err(|_| fail(StatusCode::NOT_FOUND, "no logo"))?;
    Ok(([(CONTENT_TYPE, "image/jpeg"), (CACHE_CONTROL, "no-store")], bytes).into_response())
}

/// `PUT` with the JPEG itself as the body. The dashboard converts any picture
/// (PNG, SVG, ...) to JPEG on a white background before sending it.
pub(super) async fn logo_put(State(state): State<Arc<AppState>>, body: axum::body::Bytes) -> AppResult {
    if body.len() > LOGO_LIMIT {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "the logo is limited to 1 MB"));
    }
    let jpeg = Jpeg::parse(&body).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let path = logo_path(&state).ok_or_else(|| internal("no data folder".into()))?;
    tokio::fs::write(&path, &body).await.map_err(|e| internal(format!("could not save the logo: {e}")))?;
    state.activity("info", "report", None, "Report logo replaced");
    Ok(ok(json!({"width": jpeg.width, "height": jpeg.height})))
}

pub(super) async fn logo_delete(State(state): State<Arc<AppState>>) -> AppResult {
    if let Some(path) = logo_path(&state) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => state.activity("info", "report", None, "Report logo removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(internal(format!("could not remove the logo: {e}"))),
        }
    }
    Ok(ok(json!({"removed": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe() {
        assert_eq!(slug("store front/../x"), "store-front----x");
        assert_eq!(slug("\"; rm"), "rm");
        assert_eq!(slug("///"), "project");
    }
}
