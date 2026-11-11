//! Automation: cron schedules, deploy (pull + run), auto-deploy on new
//! commits, automatic restart of crashed services and deploy webhooks.

use crate::db::Target;
use crate::git::{self, GitInfo};
use crate::runner;
use crate::state::AppState;
use crate::util::{log, now_ms};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------- cron --

/// A five-field cron expression: minute, hour, day of month, month, day of
/// week. Supports `*`, lists (`1,15`), ranges (`1-5`), steps (`*/10`,
/// `0-30/5`) and the shortcuts `@hourly`, `@daily`, `@weekly`, `@monthly`.
/// Times are evaluated in the machine's local time zone.
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    days_restricted: bool,
    weekdays_restricted: bool,
}

fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<(u64, bool), String> {
    let mut mask = 0u64;
    let mut restricted = false;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("{name}: invalid step '{s}'"))?;
                (r, step)
            }
            None => (part, 1),
        };
        let number = |s: &str| -> Result<u32, String> {
            s.parse::<u32>()
                .ok()
                .filter(|v| (min..=max).contains(v))
                .ok_or_else(|| format!("{name}: '{s}' is not between {min} and {max}"))
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (number(a)?, number(b)?)
        } else {
            let v = number(range)?;
            // `5/10` means "from 5 to the end, every 10".
            (v, if part.contains('/') { max } else { v })
        };
        if lo > hi {
            return Err(format!("{name}: range '{range}' is backwards"));
        }
        if range != "*" || step > 1 {
            restricted = true;
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok((mask, restricted))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let expr = match expr.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(
                "a schedule needs five fields: minute hour day-of-month month day-of-week".into(),
            );
        }
        let (minutes, _) = parse_field(fields[0], 0, 59, "minute")?;
        let (hours, _) = parse_field(fields[1], 0, 23, "hour")?;
        let (days, days_restricted) = parse_field(fields[2], 1, 31, "day of month")?;
        let (months, _) = parse_field(fields[3], 1, 12, "month")?;
        let (mut weekdays, weekdays_restricted) = parse_field(fields[4], 0, 7, "day of week")?;
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1; // 7 is Sunday too
        }
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_restricted,
            weekdays_restricted,
        })
    }

    fn day_matches(&self, month: u32, day: u32, weekday: u32) -> bool {
        if self.months & (1 << month) == 0 {
            return false;
        }
        let dom = self.days & (1 << day) != 0;
        let dow = self.weekdays & (1 << weekday) != 0;
        // Classic cron: when both day fields are restricted, either may match.
        if self.days_restricted && self.weekdays_restricted {
            dom || dow
        } else {
            dom && dow
        }
    }

    pub fn matches(&self, minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> bool {
        self.minutes & (1 << minute) != 0
            && self.hours & (1 << hour) != 0
            && self.day_matches(month, day, weekday)
    }

    fn matches_time(&self, t: &DateTime<Local>) -> bool {
        self.matches(
            t.minute(),
            t.hour(),
            t.day(),
            t.month(),
            t.weekday().num_days_from_sunday(),
        )
    }

    /// The first matching minute strictly after `after`, within four years.
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut t = after + chrono::Duration::seconds(60 - i64::from(after.second()));
        let limit = after + chrono::Duration::days(366 * 4);
        while t < limit {
            if !self.day_matches(t.month(), t.day(), t.weekday().num_days_from_sunday()) {
                // Jump to local midnight of the next day (computed from the
                // calendar date so daylight-saving changes cannot skip an hour).
                let midnight = t
                    .date_naive()
                    .succ_opt()
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .and_then(|n| Local.from_local_datetime(&n).earliest());
                t = match midnight {
                    Some(m) if m > t => m,
                    _ => t + chrono::Duration::minutes(24 * 60 - i64::from(t.hour() * 60 + t.minute())),
                };
                continue;
            }
            if self.matches_time(&t) {
                return Some(t);
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }
}

/// Validate a schedule and return its next run times (unix seconds).
pub fn preview(expr: &str, count: usize) -> Result<Vec<i64>, String> {
    let cron = Cron::parse(expr)?;
    let mut out = Vec::new();
    let mut t = Local::now();
    for _ in 0..count {
        match cron.next_after(t) {
            Some(next) => {
                out.push(next.timestamp());
                t = next;
            }
            None => break,
        }
    }
    if out.is_empty() {
        return Err("this schedule never runs".into());
    }
    Ok(out)
}

/// Next run of a target's schedule as unix seconds, if it has a valid one.
pub fn next_run(schedule: Option<&str>) -> Option<i64> {
    let cron = Cron::parse(schedule?).ok()?;
    cron.next_after(Local::now()).map(|t| t.timestamp())
}

// ----------------------------------------------------------------- deploy --

async fn is_running(state: &Arc<AppState>, target_id: i64) -> bool {
    state.running.lock().await.contains_key(&target_id)
}

/// Fast-forward the working tree to `origin/<branch>` (off the async
/// runtime), record it in the activity feed and refresh the cached Git state.
pub async fn pull(state: &Arc<AppState>, target: &Target) -> Result<git::PullOutcome, String> {
    let (dir, branch) = (target.working_directory.clone(), target.git_branch.clone());
    let outcome = tokio::task::spawn_blocking(move || git::pull_ff(Path::new(&dir), &branch))
        .await
        .map_err(|e| format!("git worker failed: {e}"))??;
    if outcome.updated {
        let short = |s: &Option<String>| s.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("?").to_string();
        state.activity(
            "ok",
            "git",
            Some(target.id),
            format!(
                "Pulled {} commit(s) from origin/{}: {} \u{2192} {}",
                outcome.commits,
                target.git_branch,
                short(&outcome.from),
                short(&outcome.to)
            ),
        );
    }
    git::refresh_target(
        state,
        target.id,
        target.working_directory.clone(),
        target.git_branch.clone(),
        false,
    )
    .await;
    Ok(outcome)
}

/// Deploy a target: optionally pull first, then run its script. A running
/// service is restarted; a running task is left alone and reported.
pub async fn deploy(
    state: &Arc<AppState>,
    target: &Target,
    trigger: &str,
    pull_first: bool,
) -> Result<i64, String> {
    let is_service = target.run_mode == "SERVICE";
    if !is_service && is_running(state, target.id).await {
        return Err("target is already running".into());
    }
    if pull_first && Path::new(&target.working_directory).join(".git").exists() {
        pull(state, target)
            .await
            .map_err(|e| format!("git pull failed: {e}"))?;
    }
    if is_service && is_running(state, target.id).await {
        runner::restart(state, target, trigger).await
    } else {
        runner::start_run(state, target, trigger).await
    }
}

/// Called by the Git worker after a fetch for targets with auto-deploy on:
/// when origin has new commits and a clean fast-forward is possible, pull
/// and deploy. A failure is reported once per remote commit, not every poll.
pub async fn auto_deploy(state: &Arc<AppState>, target: &Target, info: &GitInfo) {
    if !info.is_repo || info.behind == 0 || info.ahead > 0 {
        return;
    }
    let Some(remote) = info.remote_head.clone() else { return };
    let already_failed = state
        .auto_deploy_failed
        .lock()
        .map(|m| m.get(&target.id) == Some(&remote))
        .unwrap_or(false);
    if already_failed {
        return;
    }
    if target.run_mode != "SERVICE" && is_running(state, target.id).await {
        return; // try again after the next fetch
    }
    match deploy(state, target, "GIT_AUTO", true).await {
        Ok(run_id) => {
            if let Ok(mut m) = state.auto_deploy_failed.lock() {
                m.remove(&target.id);
            }
            log(&format!(
                "auto-deploy of '{}' started run {run_id} for {remote}",
                target.name
            ));
        }
        Err(e) => {
            if let Ok(mut m) = state.auto_deploy_failed.lock() {
                m.insert(target.id, remote);
            }
            log(&format!("auto-deploy of '{}' failed: {e}", target.name));
            state.activity(
                "error",
                "deploy",
                Some(target.id),
                format!("Auto-deploy skipped: {e}"),
            );
        }
    }
}

// --------------------------------------------------------------- webhooks --

/// A 128-bit random token (hex) for a deploy webhook URL. `RandomState` is
/// seeded from the operating system's random source.
pub fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(now_ms() ^ i);
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

// ----------------------------------------------------------------- worker --

async fn run_due_restarts(state: &Arc<AppState>) {
    let now = Instant::now();
    let due: Vec<i64> = {
        let mut queue = state.restart_queue.lock().unwrap_or_else(|p| p.into_inner());
        let (due, later): (Vec<_>, Vec<_>) = queue.drain(..).partition(|(_, at)| *at <= now);
        *queue = later;
        due.into_iter().map(|(id, _)| id).collect()
    };
    for id in due {
        let Ok(Some(target)) = state.db.get_target(id).await else { continue };
        if !target.auto_restart || target.run_mode != "SERVICE" || is_running(state, id).await {
            continue;
        }
        if let Err(e) = runner::start_run(state, &target, "AUTO_RESTART").await {
            state.activity("error", "restart", Some(id), format!("Automatic restart failed: {e}"));
        }
    }
}

async fn run_schedules(state: &Arc<AppState>, now: &DateTime<Local>) {
    let Ok(targets) = state.db.list_targets().await else { return };
    for target in targets {
        let Some(expr) = target.schedule.as_deref() else { continue };
        let Ok(cron) = Cron::parse(expr) else { continue };
        if !cron.matches_time(now) {
            continue;
        }
        let is_service = target.run_mode == "SERVICE";
        if !is_service && is_running(state, target.id).await {
            state.activity(
                "warn",
                "schedule",
                Some(target.id),
                "Scheduled run skipped: the previous run is still in progress",
            );
            continue;
        }
        // A scheduled service is restarted; a scheduled task is run.
        if let Err(e) = runner::restart(state, &target, "SCHEDULE").await {
            state.activity(
                "error",
                "schedule",
                Some(target.id),
                format!("Scheduled run failed to start: {e}"),
            );
        }
    }
}

/// Background worker: restarts crashed services when they fall due and fires
/// cron schedules once per matching minute.
pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let stamp = |t: &DateTime<Local>| t.timestamp() / 60;
        // Do not fire for the minute the manager happened to start in.
        let mut last_minute = stamp(&Local::now());
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            run_due_restarts(&state).await;
            let now = Local::now();
            if stamp(&now) != last_minute {
                last_minute = stamp(&now);
                run_schedules(&state, &now).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_parsing_and_matching() {
        let every = Cron::parse("* * * * *").unwrap();
        assert!(every.matches(0, 0, 1, 1, 0) && every.matches(59, 23, 31, 12, 6));

        let nightly = Cron::parse("30 2 * * *").unwrap();
        assert!(nightly.matches(30, 2, 15, 6, 3));
        assert!(!nightly.matches(31, 2, 15, 6, 3) && !nightly.matches(30, 3, 15, 6, 3));

        let steps = Cron::parse("*/15 8-17 * * 1-5").unwrap();
        assert!(steps.matches(45, 8, 10, 3, 1) && steps.matches(0, 17, 10, 3, 5));
        assert!(!steps.matches(10, 8, 10, 3, 1));
        assert!(!steps.matches(0, 18, 10, 3, 1));
        assert!(!steps.matches(0, 9, 10, 3, 0), "Sunday is outside 1-5");

        let lists = Cron::parse("0 6,18 1,15 * *").unwrap();
        assert!(lists.matches(0, 18, 15, 2, 4) && !lists.matches(0, 18, 14, 2, 4));

        // Both day fields restricted: either one matches (classic cron).
        let either = Cron::parse("0 0 13 * 5").unwrap();
        assert!(either.matches(0, 0, 13, 4, 2) && either.matches(0, 0, 20, 4, 5));
        assert!(!either.matches(0, 0, 20, 4, 2));

        assert_eq!(Cron::parse("0 0 * * 7").unwrap(), Cron::parse("0 0 * * 0,7").unwrap());
        assert!(Cron::parse("0 0 * * 7").unwrap().matches(0, 0, 1, 1, 0));
        assert_eq!(Cron::parse("@daily").unwrap(), Cron::parse("0 0 * * *").unwrap());
        assert!(Cron::parse("10/20 * * * *").unwrap().matches(50, 1, 1, 1, 1));

        for bad in ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *",
            "*/0 * * * *", "5-1 * * * *", "a * * * *", "* * * * 8", "@yearlyish"] {
            assert!(Cron::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn next_run_is_computed_in_local_time() {
        let start = Local.with_ymd_and_hms(2026, 3, 10, 14, 20, 30).unwrap();
        let next = |expr: &str| Cron::parse(expr).unwrap().next_after(start).unwrap();
        let ymdhm = |t: DateTime<Local>| (t.year(), t.month(), t.day(), t.hour(), t.minute());

        assert_eq!(ymdhm(next("* * * * *")), (2026, 3, 10, 14, 21));
        assert_eq!(ymdhm(next("0 * * * *")), (2026, 3, 10, 15, 0));
        assert_eq!(ymdhm(next("30 2 * * *")), (2026, 3, 11, 2, 30));
        assert_eq!(ymdhm(next("0 9 1 * *")), (2026, 4, 1, 9, 0));
        // 2026-03-10 is a Tuesday; the next Monday is the 16th.
        assert_eq!(ymdhm(next("15 8 * * 1")), (2026, 3, 16, 8, 15));
        assert_eq!(ymdhm(next("0 0 29 2 *")), (2028, 2, 29, 0, 0));
        assert!(Cron::parse("0 0 31 2 *").unwrap().next_after(start).is_none());

        assert_eq!(preview("*/5 * * * *", 3).unwrap().len(), 3);
        assert!(preview("0 0 31 2 *", 3).is_err());
        assert!(next_run(Some("0 2 * * *")).is_some());
        assert!(next_run(Some("nonsense")).is_none() && next_run(None).is_none());
    }

    #[test]
    fn webhook_tokens_are_long_and_distinct() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
