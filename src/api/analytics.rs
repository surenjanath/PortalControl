//! The Analytics page's one request. The work is in `crate::analytics`.

use super::*;

/// `GET /api/analytics?range=24h|7d|30d` (7 days when left out).
pub(super) async fn analytics_overview(State(state): State<Arc<AppState>>, Query(q): Params) -> AppResult {
    let now = crate::util::now_secs() as i64;
    let offset = chrono::Local::now().offset().local_minus_utc() as i64;
    let live = crate::monitor::stats();
    let body = crate::analytics::build(&state.db, &live, q.get("range").map(String::as_str), now, offset)
        .await
        .map_err(internal)?;
    Ok(ok(body))
}
