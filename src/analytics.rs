//! Analytics over everything that is stored: one answer for the Analytics
//! page (`GET /api/analytics`).
//!
//! It reads the minute rows (`metric_points`, 7 days), the run history, the
//! incidents, the AI traces and the newest security scans, and adds the
//! monitor's line counters. From those it works out figures per target and for
//! the whole machine, a reliability score with the reasons for every point
//! lost, and a list of findings (memory that keeps growing, slower answers
//! than usual, outages, crash restarts, noisy logs, hours in which failures
//! cluster).
//!
//! Nothing here is stored. The limits of the answer are the limits of the
//! tables: minute rows are kept for 7 days, 200 runs per target and the newest
//! 500 incidents.

use crate::db::{Db, MetricPoint};
use crate::{metrics, monitor};
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

/// A period the page can ask for.
pub struct Range {
    pub key: &'static str,
    pub secs: i64,
    /// Width of one point in the resource and health series.
    pub bucket: i64,
    /// Width of one bar in the runs and incidents timeline.
    pub step: i64,
}

pub fn range(key: Option<&str>) -> Range {
    match key {
        Some("24h") => Range { key: "24h", secs: 24 * 3600, bucket: 300, step: 3600 },
        Some("30d") => Range { key: "30d", secs: 30 * 24 * 3600, bucket: 1800, step: 24 * 3600 },
        _ => Range { key: "7d", secs: 7 * 24 * 3600, bucket: 1800, step: 24 * 3600 },
    }
}

/// One run from the history, without its output.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub target_id: i64,
    pub trigger: String,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub at: i64,
    /// A quick command ("Apply migrations"), not a run of the script.
    pub command: bool,
}

#[derive(Debug, Clone)]
pub struct IncidentRow {
    pub target_id: Option<i64>,
    pub trigger: String,
    pub severity: String,
    pub status: String,
    pub at: i64,
}

/// A reason the reliability score of a target is below 100.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Deduction {
    pub what: String,
    pub points: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TargetStats {
    pub id: i64,
    pub name: String,
    pub project: String,
    pub run_mode: String,
    pub has_health: bool,
    pub minutes_recorded: usize,
    pub running_minutes: usize,
    pub probes: i64,
    pub probes_up: i64,
    pub availability_pct: Option<f64>,
    pub latency_avg: Option<f64>,
    pub latency_p50: Option<f64>,
    pub latency_p95: Option<f64>,
    pub latency_max: Option<f64>,
    /// Average of the last hour and of the time before it.
    pub latency_recent: Option<f64>,
    pub latency_usual: Option<f64>,
    pub latency_anomaly: bool,
    pub cpu_avg: Option<f64>,
    pub cpu_peak: Option<f64>,
    pub mem_avg: Option<f64>,
    pub mem_peak: Option<i64>,
    /// Slope of a straight line through the memory minutes, bytes per hour,
    /// and how well the line fits (r squared, 0 to 1).
    pub mem_trend_per_hour: Option<f64>,
    pub mem_trend_fit: Option<f64>,
    pub mem_growing: bool,
    pub outages: usize,
    pub downtime_minutes: i64,
    pub longest_outage_minutes: i64,
    /// Mean time to recover and mean time between failures, in minutes.
    pub mttr_minutes: Option<f64>,
    pub mtbf_minutes: Option<f64>,
    pub runs: usize,
    pub runs_ok: usize,
    pub runs_failed: usize,
    pub crash_restarts: usize,
    pub run_avg_ms: Option<i64>,
    pub run_p95_ms: Option<i64>,
    pub incidents: usize,
    pub incidents_critical: usize,
    pub incidents_open: usize,
    pub lines: u64,
    pub error_lines: u64,
    pub scanned: bool,
    pub critical_issues: i64,
    pub vulnerable_packages: i64,
    pub score: u32,
    pub deductions: Vec<Deduction>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Insight {
    /// `err`, `warn` or `info`.
    pub level: &'static str,
    pub target_id: Option<i64>,
    pub target: String,
    pub text: String,
}

/// One bar of the timeline.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Slot {
    pub t: i64,
    pub ok: u32,
    pub failed: u32,
    pub incidents: u32,
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn megabytes(bytes: f64) -> String {
    let mb = bytes / 1_048_576.0;
    if mb.abs() >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb.abs() >= 10.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

fn minutes_text(minutes: i64) -> String {
    if minutes >= 120 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// A straight line through the memory of the minutes a target was running:
/// (bytes per hour, r squared, hours between the first and the last minute).
/// `None` with fewer than 30 minutes or when memory never changed.
pub fn memory_trend(rows: &[MetricPoint]) -> Option<(f64, f64, f64)> {
    let pts: Vec<(f64, f64)> = rows.iter().filter_map(|p| p.mem.map(|m| (p.t as f64, m as f64))).collect();
    if pts.len() < 30 {
        return None;
    }
    let t0 = pts[0].0;
    let n = pts.len() as f64;
    let xs: Vec<f64> = pts.iter().map(|(t, _)| (t - t0) / 3600.0).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = pts.iter().map(|(_, m)| m).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for (x, (_, y)) in xs.iter().zip(&pts) {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
        syy += (y - my) * (y - my);
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return None;
    }
    Some((sxy / sxx, (sxy * sxy) / (sxx * syy), xs[xs.len() - 1]))
}

/// Fill in everything that comes from a target's minute rows (oldest first).
pub fn apply_metrics(s: &mut TargetStats, rows: &[MetricPoint], now: i64) {
    s.minutes_recorded = rows.len();
    s.probes = rows.iter().map(|p| p.probes).sum();
    s.probes_up = rows.iter().map(|p| p.probes_up).sum();
    s.availability_pct = (s.probes > 0).then(|| round2(s.probes_up as f64 * 100.0 / s.probes as f64));

    let mut latencies: Vec<f64> = rows.iter().filter_map(|p| p.latency).collect();
    latencies.sort_by(|a, b| a.total_cmp(b));
    s.latency_avg = mean(&latencies).map(round1);
    s.latency_p50 = percentile(&latencies, 0.5).map(round1);
    s.latency_p95 = percentile(&latencies, 0.95).map(round1);
    s.latency_max = latencies.last().copied().map(round1);

    let cpu: Vec<f64> = rows.iter().filter_map(|p| p.cpu).collect();
    s.running_minutes = cpu.len();
    s.cpu_avg = mean(&cpu).map(round2);
    s.cpu_peak = cpu.iter().copied().reduce(f64::max).map(round2);
    let mem: Vec<f64> = rows.iter().filter_map(|p| p.mem.map(|m| m as f64)).collect();
    s.mem_avg = mean(&mem).map(f64::round);
    s.mem_peak = rows.iter().filter_map(|p| p.mem_max).max();

    let outs = metrics::outages(rows);
    s.outages = outs.len();
    s.downtime_minutes = outs.iter().map(|o| o.minutes).sum();
    s.longest_outage_minutes = outs.iter().map(|o| o.minutes).max().unwrap_or(0);
    if !outs.is_empty() {
        let probed = rows.iter().filter(|p| p.probes > 0).count() as i64;
        s.mttr_minutes = Some(round1(s.downtime_minutes as f64 / outs.len() as f64));
        s.mtbf_minutes = Some(round1((probed - s.downtime_minutes).max(0) as f64 / outs.len() as f64));
    }

    if let Some((slope, fit, hours)) = memory_trend(rows) {
        s.mem_trend_per_hour = Some(slope.round());
        s.mem_trend_fit = Some(round2(fit));
        // Growing: a steady rise (the line fits) that added a fifth of the
        // average and at least 20 MB over two hours or more.
        let gained = slope * hours;
        s.mem_growing = mem.len() >= 120
            && fit >= 0.6
            && gained >= 20.0 * 1_048_576.0
            && s.mem_avg.is_some_and(|avg| gained >= avg * 0.2);
    }

    let recent: Vec<f64> = rows.iter().filter(|p| p.t >= now - 3600).filter_map(|p| p.latency).collect();
    let usual: Vec<f64> = rows.iter().filter(|p| p.t < now - 3600).filter_map(|p| p.latency).collect();
    if recent.len() >= 5 && usual.len() >= 30 {
        let (r, u) = (mean(&recent).unwrap_or(0.0), mean(&usual).unwrap_or(0.0));
        let spread = (usual.iter().map(|v| (v - u) * (v - u)).sum::<f64>() / usual.len() as f64).sqrt();
        s.latency_recent = Some(round1(r));
        s.latency_usual = Some(round1(u));
        s.latency_anomaly = r > u + 3.0 * spread && r > u * 1.5 && r - u > 50.0;
    }
}

/// Fill in the run figures. Quick commands are counted in neither.
pub fn apply_runs(s: &mut TargetStats, runs: &[&RunRow]) {
    let done: Vec<&&RunRow> = runs.iter().filter(|r| !r.command && r.status != "RUNNING").collect();
    s.runs = done.len();
    s.runs_ok = done.iter().filter(|r| r.status == "SUCCESS").count();
    s.runs_failed = s.runs - s.runs_ok;
    s.crash_restarts = runs.iter().filter(|r| r.trigger == "AUTO_RESTART").count();
    let mut durations: Vec<f64> = done.iter().filter_map(|r| r.duration_ms.map(|d| d as f64)).collect();
    durations.sort_by(|a, b| a.total_cmp(b));
    s.run_avg_ms = mean(&durations).map(|d| d.round() as i64);
    s.run_p95_ms = percentile(&durations, 0.95).map(|d| d as i64);
}

/// The reliability score: 100 minus what the period cost, never below 0.
/// Every deduction is returned with its reason, so the page can show why.
pub fn score(s: &TargetStats) -> (u32, Vec<Deduction>) {
    let mut out: Vec<Deduction> = Vec::new();
    let mut take = |what: String, points: f64| {
        if points > 0.0 {
            out.push(Deduction { what, points: round1(points) });
        }
    };
    if let Some(a) = s.availability_pct {
        take(format!("answered {a}% of health probes"), ((100.0 - a) * 2.0).min(40.0));
    }
    if s.runs > 0 {
        take(
            format!("{} of {} runs failed", s.runs_failed, s.runs),
            s.runs_failed as f64 / s.runs as f64 * 25.0,
        );
    }
    take(format!("{} crash restart(s)", s.crash_restarts), (s.crash_restarts as f64 * 3.0).min(15.0));
    take(
        format!("{} incident(s), {} critical", s.incidents, s.incidents_critical),
        (s.incidents as f64 * 2.0 + s.incidents_critical as f64 * 2.0).min(10.0),
    );
    take(
        format!("{} critical issue(s), {} vulnerable package(s)", s.critical_issues, s.vulnerable_packages),
        (s.critical_issues as f64 * 5.0 + s.vulnerable_packages as f64).min(10.0),
    );
    let lost: f64 = out.iter().map(|d| d.points).sum();
    ((100.0 - lost).round().clamp(0.0, 100.0) as u32, out)
}

/// Start of the bar a moment falls in, in local time (`offset` = seconds
/// east of UTC), as unix seconds.
pub fn slot_start(ts: i64, step: i64, offset: i64) -> i64 {
    (ts + offset).div_euclid(step) * step - offset
}

/// Weekday (0 = Monday) and hour of a moment in local time.
pub fn weekday_hour(ts: i64, offset: i64) -> (usize, usize) {
    let local = ts + offset;
    // 1970-01-01 was a Thursday.
    ((local.div_euclid(86_400) + 3).rem_euclid(7) as usize, (local.rem_euclid(86_400) / 3600) as usize)
}

/// Runs and incidents per bar from `since` to `now`, empty bars included.
pub fn timeline(
    runs: &[RunRow],
    incidents: &[IncidentRow],
    since: i64,
    now: i64,
    step: i64,
    offset: i64,
) -> Vec<Slot> {
    let mut slots: BTreeMap<i64, Slot> = BTreeMap::new();
    let mut t = slot_start(since, step, offset);
    while t <= now {
        slots.insert(t, Slot { t, ..Default::default() });
        t += step;
    }
    for r in runs.iter().filter(|r| !r.command && r.status != "RUNNING") {
        if let Some(s) = slots.get_mut(&slot_start(r.at, step, offset)) {
            if r.status == "SUCCESS" {
                s.ok += 1;
            } else {
                s.failed += 1;
            }
        }
    }
    for i in incidents {
        if let Some(s) = slots.get_mut(&slot_start(i.at, step, offset)) {
            s.incidents += 1;
        }
    }
    slots.into_values().collect()
}

/// Failed runs and incidents by weekday (rows, Monday first) and hour.
pub fn problem_heatmap(runs: &[RunRow], incidents: &[IncidentRow], offset: i64) -> Vec<Vec<u32>> {
    let mut grid = vec![vec![0u32; 24]; 7];
    let failed = runs.iter().filter(|r| !r.command && r.status == "FAILED").map(|r| r.at);
    for at in failed.chain(incidents.iter().map(|i| i.at)) {
        let (day, hour) = weekday_hour(at, offset);
        grid[day][hour] += 1;
    }
    grid
}

/// What stands out, most serious first.
pub fn insights(targets: &[TargetStats], heat: &[Vec<u32>]) -> Vec<Insight> {
    let mut out: Vec<Insight> = Vec::new();
    let total_lines: u64 = targets.iter().map(|t| t.lines).sum();
    for t in targets {
        let mut add = |level: &'static str, text: String| {
            out.push(Insight { level, target_id: Some(t.id), target: t.name.clone(), text });
        };
        if t.critical_issues > 0 {
            add("err", format!("has {} critical issue(s) in its newest security scan", t.critical_issues));
        }
        if let (Some(a), true) = (t.availability_pct, t.outages > 0) {
            add(
                if a < 95.0 { "err" } else { "warn" },
                format!(
                    "was unreachable for {} in {} outage(s), the longest {}; it answered {a}% of its health probes{}",
                    minutes_text(t.downtime_minutes),
                    t.outages,
                    minutes_text(t.longest_outage_minutes),
                    t.mttr_minutes
                        .map(|m| format!(" and took {} on average to come back", minutes_text(m.round() as i64)))
                        .unwrap_or_default(),
                ),
            );
        }
        if t.crash_restarts >= 3 {
            add(
                if t.crash_restarts >= 10 { "err" } else { "warn" },
                format!("crashed and was restarted {} times", t.crash_restarts),
            );
        }
        if t.runs >= 3 && t.runs_failed * 10 >= t.runs * 3 {
            add("warn", format!("{} of its {} runs failed", t.runs_failed, t.runs));
        }
        if let (true, Some(slope), Some(avg)) = (t.mem_growing, t.mem_trend_per_hour, t.mem_avg) {
            add(
                "warn",
                format!(
                    "memory rises by about {} per hour and does not come back down (average {}, peak {}); \
                     this is what a memory leak looks like",
                    megabytes(slope),
                    megabytes(avg),
                    megabytes(t.mem_peak.unwrap_or(0) as f64),
                ),
            );
        }
        if let (true, Some(r), Some(u)) = (t.latency_anomaly, t.latency_recent, t.latency_usual) {
            add(
                "warn",
                format!("answered in {r:.0} ms on average over the last hour; before that it was {u:.0} ms"),
            );
        }
        if t.lines >= 1_000 && t.error_lines * 5 >= t.lines {
            add(
                "warn",
                format!(
                    "{}% of its {} log lines since the manager started look like errors",
                    t.error_lines * 100 / t.lines,
                    t.lines
                ),
            );
        }
        if targets.len() >= 3 && total_lines >= 10_000 && t.lines * 5 >= total_lines * 4 {
            add(
                "info",
                format!(
                    "printed {}% of all log lines since the manager started ({} lines)",
                    t.lines * 100 / total_lines,
                    t.lines
                ),
            );
        }
    }
    // An hour of the day in which the problems cluster.
    let by_hour: Vec<u32> = (0..24).map(|h| heat.iter().map(|day| day[h]).sum()).collect();
    let total: u32 = by_hour.iter().sum();
    if let Some((hour, n)) = by_hour.iter().enumerate().max_by_key(|(_, n)| **n) {
        if total >= 10 && n * 10 >= total * 4 {
            out.push(Insight {
                level: "info",
                target_id: None,
                target: String::new(),
                text: format!(
                    "{n} of {total} failed runs and incidents happened between {hour:02}:00 and {hour:02}:59"
                ),
            });
        }
    }
    let rank = |level: &str| match level {
        "err" => 0,
        "warn" => 1,
        _ => 2,
    };
    out.sort_by_key(|i| rank(i.level));
    out
}

/// The machine as a whole per bucket: resource use added up over the
/// targets, health probes added up, response time averaged.
fn fleet_series(by_target: &HashMap<i64, Vec<MetricPoint>>, bucket: i64) -> Vec<Value> {
    #[derive(Default)]
    struct Acc {
        cpu: f64,
        mem: f64,
        running: u32,
        probes: i64,
        up: i64,
        latency: f64,
        answered: u32,
    }
    let mut slots: BTreeMap<i64, Acc> = BTreeMap::new();
    for rows in by_target.values() {
        for p in metrics::bucketed(rows, bucket) {
            let a = slots.entry(p.t).or_default();
            if let Some(cpu) = p.cpu {
                a.cpu += cpu;
                a.running += 1;
            }
            a.mem += p.mem.unwrap_or(0) as f64;
            a.probes += p.probes;
            a.up += p.probes_up;
            if let Some(l) = p.latency {
                a.latency += l;
                a.answered += 1;
            }
        }
    }
    slots
        .into_iter()
        .map(|(t, a)| {
            json!({
                "t": t,
                "cpu": (a.running > 0).then(|| round2(a.cpu)),
                "mem": (a.running > 0).then_some(a.mem.round()),
                "running": a.running,
                "availability": (a.probes > 0).then(|| round2(a.up as f64 * 100.0 / a.probes as f64)),
                "latency": (a.answered > 0).then(|| round1(a.latency / a.answered as f64)),
            })
        })
        .collect()
}

async fn load_runs(db: &Db, since: i64) -> Result<Vec<RunRow>, String> {
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT target_id, triggered_by, status, duration_ms, \
             CAST(strftime('%s', executed_at) AS INTEGER), label FROM process_execution_history \
             WHERE target_id IS NOT NULL AND executed_at >= datetime(?1, 'unixepoch') ORDER BY id",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok(RunRow {
                target_id: r.get(0)?,
                trigger: r.get(1)?,
                status: r.get(2)?,
                duration_ms: r.get(3)?,
                at: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                command: r.get::<_, Option<String>>(5)?.is_some_and(|l| !l.is_empty()),
            })
        })?;
        rows.collect()
    })
    .await
}

async fn load_incidents(db: &Db, since: i64) -> Result<Vec<IncidentRow>, String> {
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT target_id, trigger, severity, status, CAST(strftime('%s', created_at) AS INTEGER) \
             FROM incidents WHERE created_at >= datetime(?1, 'unixepoch') ORDER BY id",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok(IncidentRow {
                target_id: r.get(0)?,
                trigger: r.get(1)?,
                severity: r.get(2)?,
                status: r.get(3)?,
                at: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            })
        })?;
        rows.collect()
    })
    .await
}

async fn load_points(db: &Db, since: i64) -> Result<HashMap<i64, Vec<MetricPoint>>, String> {
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT target_id, t, cpu, mem, mem_max, procs, latency, probes, probes_up \
             FROM metric_points WHERE t >= ?1 ORDER BY target_id, t",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok(MetricPoint {
                target_id: r.get(0)?,
                t: r.get(1)?,
                cpu: r.get(2)?,
                mem: r.get(3)?,
                mem_max: r.get(4)?,
                procs: r.get(5)?,
                latency: r.get(6)?,
                probes: r.get(7)?,
                probes_up: r.get(8)?,
            })
        })?;
        let mut out: HashMap<i64, Vec<MetricPoint>> = HashMap::new();
        for row in rows {
            let p = row?;
            out.entry(p.target_id).or_default().push(p);
        }
        Ok(out)
    })
    .await
}

/// What the AI stage did in the period, from the traces.
async fn load_ai(db: &Db, since: i64) -> Result<Value, String> {
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT status, COUNT(*), AVG(duration_ms), COALESCE(SUM(prompt_tokens), 0), \
             COALESCE(SUM(answer_tokens), 0) FROM ai_traces \
             WHERE created_at >= datetime(?1, 'unixepoch') GROUP BY status",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let (mut answered, mut failed, mut rules) = (0i64, 0i64, 0i64);
        let (mut avg_ms, mut tokens_in, mut tokens_out) = (None::<f64>, 0i64, 0i64);
        for row in rows {
            let (status, n, avg, tin, tout) = row?;
            match status.as_str() {
                "ok" => {
                    answered = n;
                    avg_ms = avg;
                }
                "failed" => failed = n,
                _ => rules += n,
            }
            tokens_in += tin;
            tokens_out += tout;
        }
        Ok(json!({
            "answered": answered,
            "failed": failed,
            "rules_only": rules,
            "avg_ms": avg_ms.map(f64::round),
            "prompt_tokens": tokens_in,
            "answer_tokens": tokens_out,
        }))
    })
    .await
}

fn counted(map: BTreeMap<String, (u32, u32)>) -> Vec<Value> {
    let mut list: Vec<(String, (u32, u32))> = map.into_iter().collect();
    list.sort_by_key(|(_, (a, b))| std::cmp::Reverse(a + b));
    list.into_iter().map(|(key, (a, b))| json!({"key": key, "a": a, "b": b})).collect()
}

/// The whole answer of `GET /api/analytics`. `offset` is the local time
/// zone in seconds east of UTC; days and hours are local.
pub async fn build(
    db: &Db,
    live: &monitor::Stats,
    range_key: Option<&str>,
    now: i64,
    offset: i64,
) -> Result<Value, String> {
    let range = range(range_key);
    let since = now - range.secs;
    let targets = db.list_targets().await?;
    let scans = db.latest_scan_summaries().await?;
    let points = load_points(db, since).await?;
    let runs = load_runs(db, since).await?;
    let incidents = load_incidents(db, since).await?;
    let ai = load_ai(db, since).await?;
    let open_incidents = db.open_incident_count().await?;

    let mut stats: Vec<TargetStats> = Vec::with_capacity(targets.len());
    for t in &targets {
        let mut s = TargetStats {
            id: t.id,
            name: t.name.clone(),
            project: t.project.clone(),
            run_mode: t.run_mode.clone(),
            has_health: t.health_url.is_some(),
            ..Default::default()
        };
        apply_metrics(&mut s, points.get(&t.id).map(Vec::as_slice).unwrap_or(&[]), now);
        let mine: Vec<&RunRow> = runs.iter().filter(|r| r.target_id == t.id).collect();
        apply_runs(&mut s, &mine);
        for i in incidents.iter().filter(|i| i.target_id == Some(t.id)) {
            s.incidents += 1;
            s.incidents_critical += (i.severity == "critical") as usize;
            s.incidents_open += (i.status == "open") as usize;
        }
        if let Some((lines, errors, _, _)) = live.targets.get(&t.id) {
            s.lines = *lines;
            s.error_lines = *errors;
        }
        if let Some(scan) = scans.get(&t.id) {
            s.scanned = true;
            s.critical_issues = scan.critical_issues_found;
            s.vulnerable_packages = scan.vulnerable_packages_found;
        }
        let (points_left, deductions) = score(&s);
        s.score = points_left;
        s.deductions = deductions;
        stats.push(s);
    }

    let heat = problem_heatmap(&runs, &incidents, offset);
    let found = insights(&stats, &heat);

    // Figures for the machine as a whole.
    let probes: i64 = stats.iter().map(|s| s.probes).sum();
    let up: i64 = stats.iter().map(|s| s.probes_up).sum();
    let mut latencies: Vec<f64> = points.values().flatten().filter_map(|p| p.latency).collect();
    latencies.sort_by(|a, b| a.total_cmp(b));
    let total_runs: usize = stats.iter().map(|s| s.runs).sum();
    let ok_runs: usize = stats.iter().map(|s| s.runs_ok).sum();
    let scores: Vec<f64> = stats.iter().map(|s| s.score as f64).collect();

    let mut by_trigger: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for r in runs.iter().filter(|r| !r.command && r.status != "RUNNING") {
        let e = by_trigger.entry(r.trigger.clone()).or_default();
        if r.status == "SUCCESS" {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    let mut by_kind: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut by_severity: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for i in &incidents {
        let open = i.status == "open";
        for (map, key) in [(&mut by_kind, &i.trigger), (&mut by_severity, &i.severity)] {
            let e = map.entry(key.clone()).or_default();
            if open {
                e.1 += 1;
            } else {
                e.0 += 1;
            }
        }
    }

    let mut grouped: BTreeMap<String, Vec<&TargetStats>> = BTreeMap::new();
    for s in &stats {
        grouped.entry(s.project.clone()).or_default().push(s);
    }
    let projects: Vec<Value> = grouped
        .into_iter()
        .map(|(name, list)| {
            let probes: i64 = list.iter().map(|s| s.probes).sum();
            let up: i64 = list.iter().map(|s| s.probes_up).sum();
            let scores: Vec<f64> = list.iter().map(|s| s.score as f64).collect();
            json!({
                "name": name,
                "targets": list.len(),
                "score": mean(&scores).map(f64::round),
                "availability_pct": (probes > 0).then(|| round2(up as f64 * 100.0 / probes as f64)),
                "downtime_minutes": list.iter().map(|s| s.downtime_minutes).sum::<i64>(),
                "runs": list.iter().map(|s| s.runs).sum::<usize>(),
                "runs_failed": list.iter().map(|s| s.runs_failed).sum::<usize>(),
                "crash_restarts": list.iter().map(|s| s.crash_restarts).sum::<usize>(),
                "incidents": list.iter().map(|s| s.incidents).sum::<usize>(),
                "lines": list.iter().map(|s| s.lines).sum::<u64>(),
            })
        })
        .collect();

    Ok(json!({
        "range": range.key,
        "range_secs": range.secs,
        "bucket_secs": range.bucket,
        "step_secs": range.step,
        "generated_at": now,
        "since": since,
        // Minute rows are kept for 7 days, so a longer period has resource
        // and health figures for its last 7 days only.
        "metrics_from": points.values().filter_map(|rows| rows.first().map(|p| p.t)).min(),
        "metrics_kept_secs": metrics::RETENTION_SECS,
        "summary": {
            "targets": stats.len(),
            "score": mean(&scores).map(f64::round),
            "availability_pct": (probes > 0).then(|| round2(up as f64 * 100.0 / probes as f64)),
            "latency_p50": percentile(&latencies, 0.5).map(round1),
            "latency_p95": percentile(&latencies, 0.95).map(round1),
            "outages": stats.iter().map(|s| s.outages).sum::<usize>(),
            "downtime_minutes": stats.iter().map(|s| s.downtime_minutes).sum::<i64>(),
            "runs": total_runs,
            "runs_ok": ok_runs,
            "runs_failed": total_runs - ok_runs,
            "success_pct": (total_runs > 0).then(|| round1(ok_runs as f64 * 100.0 / total_runs as f64)),
            "crash_restarts": stats.iter().map(|s| s.crash_restarts).sum::<usize>(),
            "incidents": incidents.len(),
            "incidents_open": open_incidents,
            "critical_issues": stats.iter().map(|s| s.critical_issues).sum::<i64>(),
            "vulnerable_packages": stats.iter().map(|s| s.vulnerable_packages).sum::<i64>(),
            "scanned": stats.iter().filter(|s| s.scanned).count(),
        },
        "monitor": {
            "lines": live.lines,
            "error_lines": live.error_lines,
            "flagged": live.flagged,
            "requests": live.requests,
            "threats": live.threats,
            "emails": live.emails,
        },
        "ai": ai,
        "series": fleet_series(&points, range.bucket),
        "timeline": timeline(&runs, &incidents, since, now, range.step, offset),
        "heatmap": heat,
        "runs_by_trigger": counted(by_trigger),
        "incidents_by_kind": counted(by_kind),
        "incidents_by_severity": counted(by_severity),
        "projects": projects,
        "targets": stats,
        "insights": found,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TargetInput;

    fn point(t: i64, mem: Option<i64>, latency: Option<f64>, probes: i64, up: i64) -> MetricPoint {
        MetricPoint {
            target_id: 1,
            t,
            cpu: mem.map(|_| 2.0),
            mem,
            mem_max: mem,
            procs: mem.map(|_| 1),
            latency,
            probes,
            probes_up: up,
        }
    }

    fn run(target_id: i64, status: &str, trigger: &str, at: i64) -> RunRow {
        RunRow {
            target_id,
            trigger: trigger.into(),
            status: status.into(),
            duration_ms: Some(1_000),
            at,
            command: false,
        }
    }

    #[test]
    fn steady_memory_growth_is_found_and_a_flat_line_is_not() {
        const MB: i64 = 1_048_576;
        // Four hours, 100 MB rising by 30 MB an hour.
        let rising: Vec<MetricPoint> =
            (0..240).map(|m| point(m * 60, Some(100 * MB + m * MB / 2), None, 0, 0)).collect();
        let (slope, fit, hours) = memory_trend(&rising).unwrap();
        assert!((slope / MB as f64 - 30.0).abs() < 0.1, "slope {slope}");
        assert!(fit > 0.99 && (hours - 239.0 / 60.0).abs() < 0.01);
        let mut s = TargetStats::default();
        apply_metrics(&mut s, &rising, 240 * 60);
        assert!(s.mem_growing);
        assert_eq!(s.running_minutes, 240);

        // The same range of values going up and down is not growth.
        let saw: Vec<MetricPoint> =
            (0..240).map(|m| point(m * 60, Some(100 * MB + (m % 20) * 6 * MB), None, 0, 0)).collect();
        let mut s = TargetStats::default();
        apply_metrics(&mut s, &saw, 240 * 60);
        assert!(!s.mem_growing, "fit {:?}", s.mem_trend_fit);

        let flat: Vec<MetricPoint> = (0..240).map(|m| point(m * 60, Some(100 * MB), None, 0, 0)).collect();
        assert!(memory_trend(&flat).is_none());
        assert!(memory_trend(&rising[..10]).is_none());
    }

    #[test]
    fn availability_outages_and_recovery_times() {
        // 100 probed minutes, of which minutes 10..20 and 50..55 were down.
        let rows: Vec<MetricPoint> = (0..100)
            .map(|m| {
                let down = (10..20).contains(&m) || (50..55).contains(&m);
                point(m * 60, None, (!down).then_some(40.0), 4, if down { 0 } else { 4 })
            })
            .collect();
        let mut s = TargetStats::default();
        apply_metrics(&mut s, &rows, 100 * 60);
        assert_eq!(s.availability_pct, Some(85.0));
        assert_eq!((s.outages, s.downtime_minutes, s.longest_outage_minutes), (2, 15, 10));
        assert_eq!(s.mttr_minutes, Some(7.5));
        assert_eq!(s.mtbf_minutes, Some(42.5));
        assert_eq!(s.latency_p50, Some(40.0));
        assert_eq!(s.running_minutes, 0);
    }

    #[test]
    fn a_slow_last_hour_is_an_anomaly() {
        let now = 300 * 60;
        let rows: Vec<MetricPoint> = (0..300)
            .map(|m| point(m * 60, None, Some(if m >= 240 { 400.0 } else { 40.0 + (m % 5) as f64 }), 4, 4))
            .collect();
        let mut s = TargetStats::default();
        apply_metrics(&mut s, &rows, now);
        assert!(s.latency_anomaly);
        assert_eq!(s.latency_recent, Some(400.0));

        let even: Vec<MetricPoint> =
            (0..300).map(|m| point(m * 60, None, Some(40.0 + (m % 5) as f64), 4, 4)).collect();
        let mut s = TargetStats::default();
        apply_metrics(&mut s, &even, now);
        assert!(!s.latency_anomaly);
    }

    #[test]
    fn score_names_every_point_it_takes() {
        let clean = TargetStats { availability_pct: Some(100.0), runs: 4, runs_ok: 4, ..Default::default() };
        assert_eq!(score(&clean), (100, vec![]));

        let rough = TargetStats {
            availability_pct: Some(95.0),
            runs: 4,
            runs_ok: 3,
            runs_failed: 1,
            crash_restarts: 2,
            incidents: 1,
            incidents_critical: 1,
            vulnerable_packages: 3,
            ..Default::default()
        };
        let (points, why) = score(&rough);
        // 10 (availability) + 6.3 (runs) + 6 (crashes) + 4 (incident) + 3 (packages)
        assert_eq!(points, 71);
        assert_eq!(why.len(), 5);
        assert_eq!(why[0], Deduction { what: "answered 95% of health probes".into(), points: 10.0 });

        // Each part has a ceiling, so one bad figure cannot take everything.
        let down = TargetStats { availability_pct: Some(0.0), crash_restarts: 99, ..Default::default() };
        assert_eq!(score(&down).0, 45);
    }

    #[test]
    fn runs_are_counted_without_quick_commands() {
        let mut command = run(1, "FAILED", "COMMAND", 10);
        command.command = true;
        let rows = [
            run(1, "SUCCESS", "MANUAL", 10),
            run(1, "FAILED", "AUTO_RESTART", 20),
            run(1, "RUNNING", "AUTO_RESTART", 30),
            command,
        ];
        let mut s = TargetStats::default();
        apply_runs(&mut s, &rows.iter().collect::<Vec<_>>());
        assert_eq!((s.runs, s.runs_ok, s.runs_failed, s.crash_restarts), (2, 1, 1, 2));
        assert_eq!(s.run_avg_ms, Some(1_000));
    }

    #[test]
    fn timeline_and_heatmap_use_local_time() {
        // 2026-11-16 was a Monday. 02:30 UTC is 23:30 on Sunday at UTC-3.
        let monday_0230 = 1_794_796_200;
        assert_eq!(weekday_hour(monday_0230, 0), (0, 2));
        assert_eq!(weekday_hour(monday_0230, -3 * 3600), (6, 23));

        let day = 86_400;
        let offset = -3 * 3600;
        let start = slot_start(monday_0230, day, offset);
        assert_eq!(start, monday_0230 - 23 * 3600 - 1800, "Sunday 00:00 local");

        let runs = [run(1, "FAILED", "MANUAL", monday_0230), run(1, "SUCCESS", "MANUAL", monday_0230 + day)];
        let incidents = [IncidentRow {
            target_id: Some(1),
            trigger: "failure".into(),
            severity: "critical".into(),
            status: "open".into(),
            at: monday_0230,
        }];
        let bars = timeline(&runs, &incidents, monday_0230 - day, monday_0230 + day, day, offset);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[1], Slot { t: start, ok: 0, failed: 1, incidents: 1 });
        assert_eq!((bars[2].ok, bars[2].failed), (1, 0));

        let heat = problem_heatmap(&runs, &incidents, offset);
        assert_eq!(heat[6][23], 2);
        assert_eq!(heat.iter().flatten().sum::<u32>(), 2);
    }

    #[test]
    fn insights_put_the_serious_ones_first() {
        let noisy = TargetStats {
            id: 1,
            name: "uptime".into(),
            lines: 9_000,
            error_lines: 3_000,
            ..Default::default()
        };
        let broken = TargetStats {
            id: 2,
            name: "claims".into(),
            availability_pct: Some(80.0),
            outages: 2,
            downtime_minutes: 130,
            longest_outage_minutes: 90,
            mttr_minutes: Some(65.0),
            lines: 500,
            ..Default::default()
        };
        let quiet = TargetStats { id: 3, name: "agent".into(), lines: 500, ..Default::default() };
        let mut heat = vec![vec![0u32; 24]; 7];
        heat[2][3] = 8;
        heat[4][14] = 4;
        let found = insights(&[noisy, broken, quiet], &heat);
        assert_eq!(found[0].level, "err");
        assert_eq!(found[0].target, "claims");
        assert!(found[0].text.contains("2 h 10 min in 2 outage(s)"), "{}", found[0].text);
        assert!(found.iter().any(|i| i.target == "uptime" && i.text.contains("33% of its 9000 log lines")));
        assert!(found
            .iter()
            .any(|i| i.target == "uptime" && i.text.contains("printed 90% of all log lines")));
        let last = found.last().unwrap();
        assert_eq!(last.target_id, None);
        assert!(last.text.contains("8 of 12") && last.text.contains("03:00"), "{}", last.text);
    }

    #[tokio::test]
    async fn the_answer_is_built_from_the_database() {
        let db = Db::open_in_memory().unwrap();
        let input = TargetInput {
            name: "claims".into(),
            working_directory: "/tmp/x".into(),
            script_name: "start.sh".into(),
            project: Some("Insurance".into()),
            health_url: Some("http://127.0.0.1:8000/".into()),
            ..Default::default()
        };
        let id = db.insert_target(input).await.unwrap();
        let now = crate::util::now_secs() as i64;
        let minute = now / 60 * 60;
        let rows: Vec<MetricPoint> = (1..=60)
            .map(|m| MetricPoint {
                target_id: id,
                t: minute - m * 60,
                cpu: Some(4.0),
                mem: Some(50_000_000),
                mem_max: Some(60_000_000),
                procs: Some(2),
                latency: Some(25.0),
                probes: 4,
                probes_up: if m <= 6 { 0 } else { 4 },
            })
            .collect();
        db.insert_metric_points(rows).await.unwrap();
        let ok = db.start_run(id, "MANUAL".into(), None, None).await.unwrap();
        db.finish_run(ok, "SUCCESS", Some(0), 2_000, String::new()).await.unwrap();
        let bad = db.start_run(id, "AUTO_RESTART".into(), None, None).await.unwrap();
        db.finish_run(bad, "FAILED", Some(1), 4_000, String::new()).await.unwrap();
        db.insert_incident(crate::db::Incident {
            target_id: Some(id),
            trigger: "failure".into(),
            severity: "critical".into(),
            title: "Run failed".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.insert_scan(id, 1, 2, "{}".into()).await.unwrap();

        let mut live = monitor::Stats { lines: 120, error_lines: 6, ..Default::default() };
        live.targets.insert(id, (120, 6, 3, false));
        let v = build(&db, &live, Some("24h"), now, 0).await.unwrap();

        assert_eq!(v["range"], "24h");
        assert_eq!(v["summary"]["targets"], 1);
        assert_eq!(v["summary"]["availability_pct"], 90.0);
        assert_eq!(v["summary"]["runs"], 2);
        assert_eq!(v["summary"]["success_pct"], 50.0);
        assert_eq!(v["summary"]["incidents"], 1);
        assert_eq!(v["summary"]["incidents_open"], 1);
        assert_eq!(v["summary"]["critical_issues"], 1);
        assert_eq!(v["monitor"]["lines"], 120);
        let t = &v["targets"][0];
        assert_eq!(t["name"], "claims");
        assert_eq!((t["outages"].as_u64(), t["downtime_minutes"].as_i64()), (Some(1), Some(6)));
        assert_eq!(t["crash_restarts"], 1);
        assert_eq!(t["lines"], 120);
        assert_eq!(t["run_avg_ms"], 3_000);
        // 20 (availability) + 12.5 (runs) + 3 (crash) + 4 (incident) + 7 (scan)
        assert_eq!(t["score"], 54);
        assert_eq!(t["deductions"].as_array().unwrap().len(), 5);
        assert_eq!(v["projects"][0]["name"], "Insurance");
        assert_eq!(v["runs_by_trigger"].as_array().unwrap().len(), 2);
        assert_eq!(v["incidents_by_severity"][0], json!({"key": "critical", "a": 0, "b": 1}));
        assert!(!v["series"].as_array().unwrap().is_empty());
        assert_eq!(v["timeline"].as_array().unwrap().len(), 25);
        let problems: u64 = v["heatmap"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|day| day.as_array().unwrap())
            .filter_map(Value::as_u64)
            .sum();
        assert_eq!(problems, 2);
        assert!(v["insights"].as_array().unwrap().iter().any(|i| i["level"] == "err"));

        // Nothing registered and nothing recorded is an empty answer, not an error.
        let fresh = Db::open_in_memory().unwrap();
        let empty = build(&fresh, &monitor::Stats::default(), None, now, 0).await.unwrap();
        assert_eq!(empty["range"], "7d");
        assert_eq!(empty["summary"]["runs"], 0);
        assert!(empty["summary"]["availability_pct"].is_null());
        assert!(empty["summary"]["score"].is_null());
        assert_eq!(empty["timeline"].as_array().unwrap().len(), 8);
    }
}
