//! PDF reports: everything, or one project (see `crate::report`).

use super::*;
use crate::report;

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

    let mut recent_runs = state.db.list_recent_runs(200).await.map_err(internal)?;
    recent_runs.retain(|r| ids.contains(&r.target_id));
    recent_runs.truncate(25);

    let mut runs_by_target = HashMap::new();
    let mut scans = HashMap::new();
    let mut usage_24h = HashMap::new();
    let since = crate::util::now_secs() as i64 - 24 * 3600;
    for &id in &ids {
        if project.is_some() {
            runs_by_target.insert(id, state.db.list_runs(id, 10).await.map_err(internal)?);
            if let Some(scan) = state.db.latest_scan(id, true).await.map_err(internal)? {
                scans.insert(id, scan);
            }
        }
        let points = state.db.metric_points_since(id, since).await.map_err(internal)?;
        let cpus: Vec<f64> = points.iter().filter_map(|p| p.cpu).collect();
        let mems: Vec<i64> = points.iter().filter_map(|p| p.mem).collect();
        let avg_cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
        let avg_mem = (!mems.is_empty()).then(|| mems.iter().sum::<i64>() / mems.len() as i64);
        let peak = points.iter().filter_map(|p| p.mem_max.or(p.mem)).max();
        usage_24h.insert(id, (avg_cpu, avg_mem, peak));
    }

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
    })
}

pub(super) async fn report_overall(State(state): State<Arc<AppState>>) -> AppResult {
    let input = gather(&state, None).await?;
    let date = chrono::Local::now().format("%Y-%m-%d");
    state.activity("info", "report", None, "Overall PDF report downloaded");
    Ok(pdf_response(report::overall(&input), &format!("portalcontrol-report-{date}.pdf")))
}

pub(super) async fn report_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult {
    let input = gather(&state, Some(&name)).await?;
    let date = chrono::Local::now().format("%Y-%m-%d");
    state.activity("info", "report", None, format!("PDF report of project '{name}' downloaded"));
    Ok(pdf_response(report::project(&input), &format!("portalcontrol-{}-{date}.pdf", slug(&name))))
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
