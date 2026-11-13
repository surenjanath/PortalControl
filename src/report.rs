//! PDF reports: one for everything PortalControl supervises, and one per
//! project with a section for each of its targets. Built from the same target
//! views the dashboard uses, so a report says what the dashboard says.
//!
//! Secrets never appear: environment variables are counted, not listed, and
//! webhook tokens and secrets are left out.

use crate::db::{RunRecord, ScanReport};
use crate::pdf::{fit, text_width, wrap, Document, Font, Rgb, PAGE_H, PAGE_W};
use serde_json::Value;
use std::collections::HashMap;

const MARGIN: f64 = 40.0;
const CONTENT_W: f64 = PAGE_W - 2.0 * MARGIN;
const BOTTOM: f64 = PAGE_H - 48.0;

const BRAND: Rgb = Rgb(0x7c, 0x3a, 0xed);
const BRAND_DARK: Rgb = Rgb(0x5b, 0x21, 0xb6);
const INK: Rgb = Rgb(0x18, 0x18, 0x1b);
const MUTED: Rgb = Rgb(0x52, 0x52, 0x5b);
const DIM: Rgb = Rgb(0x8a, 0x8a, 0x94);
const LINE: Rgb = Rgb(0xe4, 0xe4, 0xe7);
const ZEBRA: Rgb = Rgb(0xfa, 0xfa, 0xfb);
const PANEL: Rgb = Rgb(0xf4, 0xf4, 0xf6);
const WHITE: Rgb = Rgb(255, 255, 255);
const OK: Rgb = Rgb(0x05, 0x96, 0x69);
const WARN: Rgb = Rgb(0xb4, 0x53, 0x09);
const ERR: Rgb = Rgb(0xdc, 0x26, 0x26);
const INFO: Rgb = Rgb(0x25, 0x63, 0xeb);

/// Average CPU %, average memory and peak memory of a target.
pub type Usage = (Option<f64>, Option<i64>, Option<i64>);

/// Everything a report is made from.
pub struct Input {
    /// `Some(name)` for a project report, `None` for the overall report.
    pub project: Option<String>,
    pub host: String,
    pub version: String,
    /// Local time the report was made, already formatted.
    pub generated: String,
    /// Target views as sent to the dashboard (`api::views::target_view`).
    pub targets: Vec<Value>,
    /// Most recent runs across the targets in the report, newest first.
    pub recent_runs: Vec<RunRecord>,
    /// Project report only: recent runs per target.
    pub runs_by_target: HashMap<i64, Vec<RunRecord>>,
    /// Project report only: the latest scan per target, with its findings.
    pub scans: HashMap<i64, ScanReport>,
    /// Average CPU %, average memory and peak memory over the last 24 hours.
    pub usage_24h: HashMap<i64, Usage>,
}

// ------------------------------------------------------------ layout --

#[derive(Clone)]
pub struct Cell {
    text: String,
    color: Rgb,
    bold: bool,
}

fn cell(text: impl Into<String>) -> Cell {
    Cell { text: text.into(), color: INK, bold: false }
}
fn colored(text: impl Into<String>, color: Rgb) -> Cell {
    Cell { text: text.into(), color, bold: false }
}
fn strong(text: impl Into<String>) -> Cell {
    Cell { text: text.into(), color: INK, bold: true }
}

struct Layout {
    doc: Document,
    y: f64,
    running_title: String,
}

impl Layout {
    fn new(title: &str, running_title: &str) -> Self {
        let mut l = Layout { doc: Document::new(title), y: 0.0, running_title: running_title.to_string() };
        l.doc.add_page();
        l
    }

    fn page(&mut self) -> &mut crate::pdf::Page {
        let i = self.doc.page_count() - 1;
        self.doc.page(i)
    }

    fn new_page(&mut self) {
        self.doc.add_page();
        let title = self.running_title.clone();
        let p = self.page();
        p.text(MARGIN, 22.0, &title, Font::Bold, 8.5, BRAND);
        p.line(MARGIN, 36.0, PAGE_W - MARGIN, 36.0, 0.6, LINE);
        self.y = 50.0;
    }

    /// Start a new page if `h` more points do not fit.
    fn need(&mut self, h: f64) -> bool {
        if self.y + h > BOTTOM {
            self.new_page();
            true
        } else {
            false
        }
    }

    fn gap(&mut self, h: f64) {
        self.y += h;
    }

    fn heading(&mut self, text: &str) {
        self.need(46.0);
        self.gap(8.0);
        let y = self.y;
        let p = self.page();
        p.rect(MARGIN, y + 2.0, 3.0, 14.0, BRAND);
        p.text(MARGIN + 10.0, y, text, Font::Bold, 14.0, INK);
        self.gap(24.0);
    }

    /// A smaller heading. Room is kept for what follows it (a table header and
    /// a couple of rows), so a heading never ends a page on its own.
    fn subheading(&mut self, text: &str, note: &str) {
        self.need(80.0);
        self.gap(4.0);
        let y = self.y;
        let p = self.page();
        p.text(MARGIN, y, text, Font::Bold, 11.0, INK);
        if !note.is_empty() {
            let w = text_width(text, Font::Bold, 11.0);
            p.text(MARGIN + w + 8.0, y + 1.5, note, Font::Regular, 9.0, MUTED);
        }
        self.gap(18.0);
    }

    fn paragraph(&mut self, text: &str, color: Rgb, size: f64) {
        for line in wrap(text, Font::Regular, size, CONTENT_W) {
            self.need(size + 4.0);
            let y = self.y;
            self.page().text(MARGIN, y, &line, Font::Regular, size, color);
            self.gap(size + 4.0);
        }
    }

    fn bullets(&mut self, items: &[(Rgb, String)]) {
        for (color, text) in items {
            let lines = wrap(text, Font::Regular, 9.5, CONTENT_W - 14.0);
            self.need(13.5 * lines.len() as f64);
            let y = self.y;
            self.page().rect(MARGIN + 1.0, y + 3.5, 5.0, 5.0, *color);
            for line in lines {
                let y = self.y;
                self.page().text(MARGIN + 14.0, y, &line, Font::Regular, 9.5, INK);
                self.gap(13.5);
            }
        }
    }

    /// A row of figure tiles: (label, value, note, color of the value).
    fn tiles(&mut self, tiles: &[(&str, String, String, Rgb)]) {
        let gap = 8.0;
        // Six tiles read best as two rows of three.
        let per_row = if tiles.len().is_multiple_of(3) { 3 } else { tiles.len().clamp(1, 4) };
        let w = (CONTENT_W - gap * (per_row as f64 - 1.0)) / per_row as f64;
        let h = 58.0;
        for row in tiles.chunks(per_row) {
            self.need(h + gap);
            let y = self.y;
            for (i, (label, value, note, color)) in row.iter().enumerate() {
                let x = MARGIN + i as f64 * (w + gap);
                let p = self.page();
                p.rect(x, y, w, h, PANEL);
                p.text(x + 10.0, y + 9.0, label, Font::Regular, 8.5, MUTED);
                p.text(x + 10.0, y + 22.0, &fit(value, Font::Bold, 17.0, w - 20.0), Font::Bold, 17.0, *color);
                p.text(x + 10.0, y + 43.0, &fit(note, Font::Regular, 7.5, w - 20.0), Font::Regular, 7.5, DIM);
            }
            self.gap(h + gap);
        }
    }

    /// Label / value pairs in two columns.
    fn facts(&mut self, rows: &[(&str, String)]) {
        let col_w = CONTENT_W / 2.0;
        let label_w = 92.0;
        for pair in rows.chunks(2) {
            let wrapped: Vec<Vec<String>> =
                pair.iter().map(|(_, v)| wrap(v, Font::Regular, 8.5, col_w - label_w - 12.0)).collect();
            let lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).min(3);
            let h = 12.0 * lines as f64 + 3.0;
            self.need(h);
            let y = self.y;
            for (i, ((label, _), vlines)) in pair.iter().zip(&wrapped).enumerate() {
                let x = MARGIN + i as f64 * col_w;
                let p = self.page();
                p.text(x, y, label, Font::Regular, 8.5, MUTED);
                for (n, line) in vlines.iter().take(3).enumerate() {
                    p.text(x + label_w, y + 12.0 * n as f64, line, Font::Regular, 8.5, INK);
                }
            }
            self.gap(h);
        }
    }

    /// A table with a header row that repeats after a page break. Column
    /// widths are fractions of the content width.
    fn table(&mut self, cols: &[(&str, f64)], rows: &[Vec<Cell>]) {
        let size = 8.5;
        let row_h = 17.0;
        let widths: Vec<f64> = cols.iter().map(|(_, f)| f * CONTENT_W).collect();
        let header = |l: &mut Layout| {
            let y = l.y;
            let p = l.page();
            p.rect(MARGIN, y, CONTENT_W, row_h, BRAND);
            let mut x = MARGIN;
            for ((title, _), w) in cols.iter().zip(&widths) {
                p.text(x + 6.0, y + 5.0, &fit(title, Font::Bold, 8.0, w - 10.0), Font::Bold, 8.0, WHITE);
                x += w;
            }
            l.gap(row_h);
        };
        self.need(row_h * 2.0);
        header(self);
        for (n, row) in rows.iter().enumerate() {
            if self.need(row_h) {
                header(self);
            }
            let y = self.y;
            let p = self.page();
            if n % 2 == 1 {
                p.rect(MARGIN, y, CONTENT_W, row_h, ZEBRA);
            }
            let mut x = MARGIN;
            for (c, w) in row.iter().zip(&widths) {
                let font = if c.bold { Font::Bold } else { Font::Regular };
                p.text(x + 6.0, y + 4.5, &fit(&c.text, font, size, w - 10.0), font, size, c.color);
                x += w;
            }
            p.line(MARGIN, y + row_h, MARGIN + CONTENT_W, y + row_h, 0.4, LINE);
            self.gap(row_h);
        }
        if rows.is_empty() {
            self.need(row_h);
            let y = self.y;
            self.page().text(MARGIN + 6.0, y + 4.5, "Nothing to show.", Font::Regular, size, DIM);
            self.gap(row_h);
        }
        self.gap(10.0);
    }

    fn finish(mut self, footer: &str) -> Vec<u8> {
        let total = self.doc.page_count();
        for i in 0..total {
            let p = self.doc.page(i);
            p.line(MARGIN, PAGE_H - 34.0, PAGE_W - MARGIN, PAGE_H - 34.0, 0.6, LINE);
            p.text(MARGIN, PAGE_H - 27.0, footer, Font::Regular, 7.5, DIM);
            p.text_right(
                PAGE_W - MARGIN,
                PAGE_H - 27.0,
                &format!("Page {} of {total}", i + 1),
                Font::Regular,
                7.5,
                DIM,
            );
        }
        self.doc.finish()
    }
}

// ----------------------------------------------------- reading the views --

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn i(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The status text and colour the dashboard shows (`tStatus` in core.js).
pub fn status(t: &Value) -> (&'static str, Rgb) {
    let health = t.get("health").and_then(|h| h.get("status")).and_then(Value::as_str).unwrap_or("");
    let service = s(t, "run_mode") == "SERVICE";
    let has_url = !s(t, "health_url").is_empty();
    let last = t.get("last_run").and_then(|r| r.get("status")).and_then(Value::as_str);
    if t.get("running").is_some_and(|r| !r.is_null()) {
        if !service {
            return ("Deploying", INFO);
        }
        return match (has_url, health) {
            (false, _) => ("Running", OK),
            (true, "LIVE") => ("Running (healthy)", OK),
            (true, "SLOW") => ("Running (slow)", WARN),
            (true, "DOWN") => ("Running (unhealthy)", WARN),
            _ => ("Running (checking)", INFO),
        };
    }
    if t.get("restart_pending").and_then(Value::as_bool).unwrap_or(false) {
        return ("Restarting", WARN);
    }
    if service {
        if health == "LIVE" || health == "SLOW" {
            return ("Up, not supervised", WARN);
        }
        if last == Some("FAILED") {
            return ("Exited (failed)", ERR);
        }
        return ("Stopped", DIM);
    }
    match last {
        None => ("Never deployed", DIM),
        Some("SUCCESS") => ("Deployed", OK),
        Some(_) => ("Last deploy failed", ERR),
    }
}

fn git_text(t: &Value) -> (String, Rgb) {
    let Some(g) = t.get("git").filter(|g| !g.is_null()) else { return ("checking".into(), DIM) };
    if !g.get("is_repo").and_then(Value::as_bool).unwrap_or(false) {
        return ("not a repo".into(), DIM);
    }
    match s(g, "status") {
        "UPDATES_AVAILABLE" => {
            let n = i(g, "behind");
            (format!("{n} update{}", if n == 1 { "" } else { "s" }), INFO)
        }
        "DIRTY" => (format!("{} changed file(s)", i(g, "dirty_files")), WARN),
        _ => ("up to date".into(), OK),
    }
}

fn uptime_text(t: &Value) -> (String, Rgb) {
    match t.get("uptime_pct").and_then(Value::as_f64) {
        None => ("-".into(), DIM),
        Some(p) => (
            format!("{p:.1}%"),
            if p >= 99.0 {
                OK
            } else if p >= 90.0 {
                WARN
            } else {
                ERR
            },
        ),
    }
}

fn scan_counts(t: &Value) -> Option<(i64, i64)> {
    t.get("scan")
        .filter(|s| !s.is_null())
        .map(|s| (i(s, "critical_issues_found"), i(s, "vulnerable_packages_found")))
}

fn security_text(t: &Value) -> (String, Rgb) {
    match scan_counts(t) {
        None => ("not scanned".into(), DIM),
        Some((0, 0)) => ("clean".into(), OK),
        Some((c, v)) => (format!("{c} critical, {v} pkg"), if c > 0 { ERR } else { WARN }),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC (as stored) to local `YYYY-MM-DD HH:MM`.
pub fn local_time(utc: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(utc.trim(), "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(t, chrono::Utc)
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| utc.to_string())
}

fn duration(ms: Option<i64>) -> String {
    match ms {
        None => "running".into(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) if ms < 60_000 => format!("{:.1} s", ms as f64 / 1000.0),
        Some(ms) => format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

fn bytes(n: i64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.1} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.0} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

fn run_result(r: &RunRecord) -> Cell {
    match r.status.as_str() {
        "SUCCESS" => colored("Success", OK),
        "RUNNING" => colored("Running", INFO),
        _ => {
            colored(r.exit_code.map(|c| format!("Failed (exit {c})")).unwrap_or_else(|| "Failed".into()), ERR)
        }
    }
}

fn trigger(t: &str) -> &str {
    match t {
        "MANUAL" => "Manual",
        "SCHEDULE" => "Schedule",
        "GIT_AUTO" => "Auto-deploy",
        "WEBHOOK" => "Webhook",
        "AUTO_RESTART" => "Crash restart",
        "RESTART" => "Restart",
        "ROLLBACK" => "Rollback",
        "COMMAND" => "Command",
        "SYSTEM_AUTO" => "Autostart",
        other => other,
    }
}

/// What needs a person: failures, unhealthy services, updates, findings.
fn attention(targets: &[Value]) -> Vec<(Rgb, String)> {
    let mut out = Vec::new();
    for t in targets {
        let name = s(t, "name");
        let (st, color) = status(t);
        if color == ERR {
            out.push((ERR, format!("{name}: {}.", st.to_lowercase())));
        } else if st == "Running (unhealthy)" {
            out.push((WARN, format!("{name}: running but its health check does not answer.")));
        }
        if let Some((c, v)) = scan_counts(t).filter(|(c, v)| c + v > 0) {
            out.push((
                if c > 0 { ERR } else { WARN },
                format!("{name}: {c} critical security issue(s) and {v} vulnerable package(s)."),
            ));
        }
        if let Some(g) = t.get("git").filter(|g| !g.is_null()) {
            match s(g, "status") {
                "UPDATES_AVAILABLE" => out.push((
                    INFO,
                    format!("{name}: {} new commit(s) on origin, ready to pull.", i(g, "behind")),
                )),
                "DIRTY" => out.push((
                    WARN,
                    format!("{name}: {} uncommitted change(s) in the working tree.", i(g, "dirty_files")),
                )),
                _ => {}
            }
        }
        if let Some(p) = t.get("uptime_pct").and_then(Value::as_f64).filter(|p| *p < 90.0) {
            out.push((WARN, format!("{name}: reachable only {p:.1}% of recent health probes.")));
        }
    }
    out
}

fn summary_tiles(targets: &[Value]) -> Vec<(&'static str, String, String, Rgb)> {
    let total = targets.len();
    let running = targets.iter().filter(|t| t.get("running").is_some_and(|r| !r.is_null())).count();
    let services = targets.iter().filter(|t| s(t, "run_mode") == "SERVICE").count();
    let probed: Vec<&Value> = targets.iter().filter(|t| !s(t, "health_url").is_empty()).collect();
    let live = probed
        .iter()
        .filter(|t| {
            matches!(
                t.get("health").and_then(|h| h.get("status")).and_then(Value::as_str),
                Some("LIVE" | "SLOW")
            )
        })
        .count();
    let failing = targets.iter().filter(|t| status(t).1 == ERR).count();
    let updates = targets
        .iter()
        .filter(|t| {
            t.get("git").and_then(|g| g.get("status")).and_then(Value::as_str) == Some("UPDATES_AVAILABLE")
        })
        .count();
    let (crit, vuln) = targets.iter().filter_map(scan_counts).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    let scanned = targets.iter().filter(|t| scan_counts(t).is_some()).count();
    vec![
        (
            "Targets running",
            format!("{running} / {total}"),
            format!("{services} service(s), {} task(s)", total - services),
            INK,
        ),
        (
            "Health checks answering",
            format!("{live} / {}", probed.len()),
            "probed every 15 s".into(),
            if live < probed.len() { WARN } else { OK },
        ),
        (
            "Failing",
            failing.to_string(),
            "last run failed or service exited".into(),
            if failing > 0 { ERR } else { OK },
        ),
        (
            "Updates available",
            updates.to_string(),
            "targets behind origin".into(),
            if updates > 0 { INFO } else { OK },
        ),
        (
            "Critical issues",
            crit.to_string(),
            format!("high severity \u{00b7} {scanned} scanned"),
            if crit > 0 { ERR } else { OK },
        ),
        (
            "Vulnerable packages",
            vuln.to_string(),
            "dependencies with advisories".into(),
            if vuln > 0 { WARN } else { OK },
        ),
    ]
}

fn targets_table(l: &mut Layout, targets: &[Value], with_project: bool) {
    let mut cols: Vec<(&str, f64)> = vec![
        ("Target", 0.2),
        ("Type", 0.09),
        ("Status", 0.19),
        ("Git", 0.14),
        ("Uptime", 0.09),
        ("Last run", 0.15),
        ("Security", 0.14),
    ];
    if with_project {
        cols = vec![
            ("Target", 0.18),
            ("Project", 0.12),
            ("Status", 0.18),
            ("Git", 0.13),
            ("Uptime", 0.08),
            ("Last run", 0.17),
            ("Security", 0.14),
        ];
    }
    let rows: Vec<Vec<Cell>> = targets
        .iter()
        .map(|t| {
            let (st, sc) = status(t);
            let (g, gc) = git_text(t);
            let (u, uc) = uptime_text(t);
            let (sec, secc) = security_text(t);
            let last = t
                .get("last_run")
                .filter(|r| !r.is_null())
                .map(|r| local_time(s(r, "executed_at")))
                .unwrap_or_else(|| "never".into());
            let second = if with_project {
                cell(s(t, "project"))
            } else {
                cell(if s(t, "run_mode") == "SERVICE" { "Service" } else { "Task" })
            };
            vec![
                strong(s(t, "name")),
                second,
                colored(st, sc),
                colored(g, gc),
                colored(u, uc),
                cell(last),
                colored(sec, secc),
            ]
        })
        .collect();
    l.table(&cols, &rows);
}

fn runs_table(l: &mut Layout, runs: &[RunRecord], names: &HashMap<i64, String>, with_target: bool) {
    let cols: Vec<(&str, f64)> = if with_target {
        vec![
            ("When", 0.17),
            ("Target", 0.2),
            ("Trigger", 0.13),
            ("Result", 0.15),
            ("Took", 0.1),
            ("Commit", 0.25),
        ]
    } else {
        vec![("When", 0.18), ("Trigger", 0.15), ("Result", 0.17), ("Took", 0.1), ("Commit / command", 0.4)]
    };
    let rows: Vec<Vec<Cell>> = runs
        .iter()
        .map(|r| {
            let what = match (&r.label, &r.commit_sha) {
                (Some(label), _) if !label.is_empty() => label.clone(),
                (_, Some(sha)) => format!(
                    "{} {}",
                    &sha[..sha.len().min(8)],
                    r.commit_message.as_deref().unwrap_or("").lines().next().unwrap_or("")
                ),
                _ => "-".into(),
            };
            let mut row = vec![cell(local_time(&r.executed_at))];
            if with_target {
                row.push(strong(
                    names.get(&r.target_id).cloned().unwrap_or_else(|| format!("#{}", r.target_id)),
                ));
            }
            row.extend([
                cell(trigger(&r.triggered_by)),
                run_result(r),
                cell(duration(r.duration_ms)),
                colored(what, MUTED),
            ]);
            row
        })
        .collect();
    l.table(&cols, &rows);
}

fn header_band(l: &mut Layout, kicker: &str, title: &str, input: &Input) {
    let p = l.page();
    p.rect(0.0, 0.0, PAGE_W, 128.0, BRAND);
    p.rect(0.0, 124.0, PAGE_W, 4.0, BRAND_DARK);
    // The PortalControl mark: a white ring around a gold dot.
    p.ring(MARGIN + 13.0, 43.0, 10.5, 3.6, WHITE);
    p.circle(MARGIN + 13.0, 43.0, 4.2, Rgb::hex("#fcd452"));
    p.text(MARGIN + 36.0, 30.0, "PortalControl", Font::Bold, 12.0, WHITE);
    p.text(MARGIN + 36.0, 45.0, kicker, Font::Regular, 8.5, Rgb(0xe9, 0xdd, 0xff));
    p.text(MARGIN, 72.0, &fit(title, Font::Bold, 24.0, CONTENT_W), Font::Bold, 24.0, WHITE);
    let meta = format!(
        "{} \u{00b7} PortalControl {} \u{00b7} generated {}",
        input.host, input.version, input.generated
    );
    p.text(
        MARGIN,
        104.0,
        &fit(&meta, Font::Regular, 9.0, CONTENT_W),
        Font::Regular,
        9.0,
        Rgb(0xe9, 0xdd, 0xff),
    );
    l.y = 148.0;
}

fn attention_section(l: &mut Layout, targets: &[Value]) {
    l.heading("Needs attention");
    let items = attention(targets);
    if items.is_empty() {
        l.bullets(&[(
            OK,
            "Nothing needs attention: everything is running, healthy, up to date and clean.".into(),
        )]);
    } else {
        l.bullets(&items);
    }
    l.gap(6.0);
}

/// The report for everything PortalControl supervises.
pub fn overall(input: &Input) -> Vec<u8> {
    let mut l = Layout::new("PortalControl report", "PortalControl \u{00b7} Overall report");
    header_band(&mut l, "Operations report", "All projects", input);
    l.heading("Summary");
    l.tiles(&summary_tiles(&input.targets));
    attention_section(&mut l, &input.targets);

    l.heading("Projects");
    let mut projects: Vec<&str> = input.targets.iter().map(|t| s(t, "project")).collect();
    projects.sort_unstable();
    projects.dedup();
    let rows: Vec<Vec<Cell>> = projects
        .iter()
        .map(|p| {
            let ts: Vec<&Value> = input.targets.iter().filter(|t| s(t, "project") == *p).collect();
            let running = ts.iter().filter(|t| t.get("running").is_some_and(|r| !r.is_null())).count();
            let failing = ts.iter().filter(|t| status(t).1 == ERR).count();
            let (c, v) = ts.iter().filter_map(|t| scan_counts(t)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            vec![
                strong(*p),
                cell(ts.len().to_string()),
                cell(format!("{running} running")),
                if failing > 0 {
                    colored(format!("{failing} failing"), ERR)
                } else {
                    colored("none failing", OK)
                },
                if c + v > 0 {
                    colored(format!("{c} critical, {v} pkg"), if c > 0 { ERR } else { WARN })
                } else {
                    colored("clean", OK)
                },
            ]
        })
        .collect();
    l.table(
        &[("Project", 0.3), ("Targets", 0.12), ("Running", 0.18), ("Failures", 0.18), ("Security", 0.22)],
        &rows,
    );

    l.heading("Targets");
    targets_table(&mut l, &input.targets, true);

    l.heading("Recent deployments");
    let names: HashMap<i64, String> =
        input.targets.iter().map(|t| (i(t, "id"), s(t, "name").to_string())).collect();
    runs_table(&mut l, &input.recent_runs, &names, true);

    l.finish(&format!("PortalControl overall report \u{00b7} {}", input.generated))
}

/// The report for one project, with a section per target.
pub fn project(input: &Input) -> Vec<u8> {
    let name = input.project.clone().unwrap_or_default();
    let mut l = Layout::new(
        &format!("{name} - PortalControl report"),
        &format!("PortalControl \u{00b7} Project {name}"),
    );
    header_band(&mut l, "Project report", &name, input);
    l.heading("Summary");
    l.tiles(&summary_tiles(&input.targets));
    attention_section(&mut l, &input.targets);
    l.heading("Processes");
    targets_table(&mut l, &input.targets, false);

    for t in &input.targets {
        let id = i(t, "id");
        l.need(160.0);
        l.heading(s(t, "name"));
        let (st, _) = status(t);
        let desc = s(t, "description");
        if !desc.is_empty() {
            l.paragraph(desc, MUTED, 9.5);
            l.gap(2.0);
        }
        let git = t.get("git").filter(|g| !g.is_null());
        let commit = git
            .and_then(|g| {
                g.get("head").and_then(Value::as_str).map(|h| {
                    format!(
                        "{} {}",
                        &h[..h.len().min(8)],
                        g.get("head_summary").and_then(Value::as_str).unwrap_or("")
                    )
                })
            })
            .unwrap_or_else(|| "-".into());
        let health = if s(t, "health_url").is_empty() {
            "no health check".to_string()
        } else {
            let latency = t
                .get("health")
                .and_then(|h| h.get("latency_ms"))
                .and_then(Value::as_i64)
                .map(|ms| format!(", {ms} ms"))
                .unwrap_or_default();
            format!("{} ({}{latency})", s(t, "health_url"), uptime_text(t).0)
        };
        let usage = match input.usage_24h.get(&id) {
            Some((cpu, mem, peak)) if cpu.is_some() || mem.is_some() => format!(
                "CPU {} avg, memory {} avg, {} peak",
                cpu.map(|c| format!("{c:.1}%")).unwrap_or("-".into()),
                mem.map(bytes).unwrap_or("-".into()),
                peak.map(bytes).unwrap_or("-".into())
            ),
            _ => "no samples in the last 24 hours".into(),
        };
        let schedule = match t.get("schedule").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => {
                let next = t
                    .get("next_run")
                    .and_then(Value::as_str)
                    .map(|n| format!(", next {}", local_time(n)))
                    .unwrap_or_default();
                format!("{c}{next}")
            }
            _ => "none".into(),
        };
        let env_count = i(t, "env_count");
        l.facts(&[
            ("Status", st.to_string()),
            (
                "Mode",
                if s(t, "run_mode") == "SERVICE" {
                    "Service (long-running)".into()
                } else {
                    "Task (runs to completion)".into()
                },
            ),
            ("Folder", s(t, "working_directory").to_string()),
            ("Script", s(t, "script_name").to_string()),
            ("Branch", format!("{} \u{00b7} {}", s(t, "git_branch"), git_text(t).0)),
            ("Commit", commit),
            ("Health", health),
            ("Resources, 24 h", usage),
            ("Schedule", schedule),
            ("Start order", i(t, "start_order").to_string()),
            (
                "Python",
                t.get("python_env")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .unwrap_or("none")
                    .to_string(),
            ),
            ("Environment", format!("{env_count} variable(s), values not shown")),
            (
                "Auto-restart",
                if t.get("auto_restart").and_then(Value::as_bool).unwrap_or(false) {
                    "on".into()
                } else {
                    "off".into()
                },
            ),
            (
                "Auto-deploy",
                if t.get("auto_deploy").and_then(Value::as_bool).unwrap_or(false) {
                    "on".into()
                } else {
                    "off".into()
                },
            ),
        ]);
        l.gap(6.0);

        l.subheading("Recent runs", "newest first");
        let runs = input.runs_by_target.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        runs_table(&mut l, runs, &HashMap::new(), false);

        match input.scans.get(&id) {
            None => {
                l.subheading("Security", "");
                l.paragraph("No security scan yet. Run one from the target's Security tab.", DIM, 9.0);
                l.gap(8.0);
            }
            Some(scan) => {
                l.subheading(
                    "Security",
                    &format!(
                        "scanned {}: {} critical issue(s), {} vulnerable package(s)",
                        local_time(&scan.scanned_at),
                        scan.critical_issues_found,
                        scan.vulnerable_packages_found
                    ),
                );
                security_findings(&mut l, scan, s(t, "working_directory"));
            }
        }
    }
    l.finish(&format!("PortalControl project report: {name} \u{00b7} {}", input.generated))
}

/// The first line of a tool error, shortened: the full text is on the
/// target's Security tab.
fn brief(err: &str) -> String {
    let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = first.chars().take(220).collect();
    if out.len() < first.len() {
        out.push('\u{2026}');
    }
    format!("{out} (details on the Security tab)")
}

fn security_findings(l: &mut Layout, scan: &ScanReport, folder: &str) {
    let payload = scan.raw_payload.clone().unwrap_or(Value::Null);
    let rank = |sev: &str| match sev {
        "HIGH" => 0,
        "MEDIUM" => 1,
        _ => 2,
    };
    let mut issues: Vec<&Value> = payload
        .get("bandit")
        .and_then(|b| b.get("results"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    issues.sort_by_key(|r| rank(s(r, "issue_severity")));
    if let Some(err) = payload.get("bandit_error").and_then(Value::as_str) {
        l.paragraph(&format!("Static analysis did not run: {}", brief(err)), WARN, 8.5);
    } else if issues.is_empty() {
        l.paragraph("Static analysis (bandit): no issues found.", OK, 9.0);
    } else {
        let shown = issues.len().min(12);
        let rows: Vec<Vec<Cell>> = issues[..shown]
            .iter()
            .map(|r| {
                let sev = s(r, "issue_severity");
                let color = match sev {
                    "HIGH" => ERR,
                    "MEDIUM" => WARN,
                    _ => MUTED,
                };
                // Paths relative to the project folder read better and fit.
                let full = s(r, "filename");
                let file = full
                    .strip_prefix(folder)
                    .map(|rest| rest.trim_start_matches(['/', '\\']))
                    .unwrap_or(full)
                    .trim_start_matches("./");
                vec![
                    Cell { text: sev.to_string(), color, bold: true },
                    cell(s(r, "test_id")),
                    cell(format!("{file}:{}", i(r, "line_number"))),
                    colored(s(r, "issue_text"), MUTED),
                ]
            })
            .collect();
        l.table(&[("Severity", 0.11), ("Test", 0.08), ("Location", 0.28), ("Issue", 0.53)], &rows);
        if issues.len() > shown {
            l.paragraph(
                &format!("{} more issue(s) are listed on the Security tab.", issues.len() - shown),
                DIM,
                8.5,
            );
        }
    }
    let deps: Vec<&Value> = payload
        .get("pip_audit")
        .and_then(|p| p.get("dependencies").or(Some(p)))
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .filter(|p| p.get("vulns").and_then(Value::as_array).is_some_and(|v| !v.is_empty()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(err) = payload.get("pip_audit_error").and_then(Value::as_str) {
        l.paragraph(&format!("Dependency audit did not run: {}", brief(err)), WARN, 8.5);
    } else if deps.is_empty() {
        l.paragraph("Dependency audit (pip-audit): no known-vulnerable packages.", OK, 9.0);
    } else {
        let rows: Vec<Vec<Cell>> = deps
            .iter()
            .map(|d| {
                let vulns = d.get("vulns").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut fixes: Vec<String> = vulns
                    .iter()
                    .filter_map(|v| v.get("fix_versions").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect();
                fixes.sort();
                fixes.dedup();
                let fix = fixes.last().cloned().unwrap_or_else(|| "no fix yet".into());
                vec![
                    strong(s(d, "name")),
                    cell(s(d, "version")),
                    colored(format!("{} advisory(ies)", vulns.len()), WARN),
                    cell(format!("upgrade to {fix} or later")),
                ]
            })
            .collect();
        l.table(&[("Package", 0.25), ("Installed", 0.15), ("Advisories", 0.25), ("Fix", 0.35)], &rows);
    }
    l.gap(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(id: i64, name: &str, project: &str, mode: &str, running: bool, last: Option<&str>) -> Value {
        json!({
            "id": id, "name": name, "project": project, "run_mode": mode,
            "running": if running { json!({"pid": 1}) } else { Value::Null },
            "last_run": last.map(|s| json!({"status": s, "executed_at": "2026-11-12 10:00:00"})),
            "health_url": if mode == "SERVICE" { "http://127.0.0.1:8000/" } else { "" },
            "health": {"status": "LIVE", "latency_ms": 3}, "uptime_pct": 99.5,
            "git": {"is_repo": true, "status": "UPDATES_AVAILABLE", "behind": 2, "head": "abcdef1234", "head_summary": "Fix"},
            "scan": {"critical_issues_found": 2, "vulnerable_packages_found": 5},
            "working_directory": "/srv/app", "script_name": "deploy.sh", "git_branch": "main",
            "env_count": 3, "env_vars": "SECRET=hunter2", "webhook_token": "tok123", "description": "Shop"
        })
    }

    fn input(project: Option<&str>) -> Input {
        Input {
            project: project.map(str::to_string),
            host: "host".into(),
            version: "0.9.0".into(),
            generated: "2026-11-12 20:00".into(),
            targets: vec![
                target(1, "web", "shop", "SERVICE", true, Some("SUCCESS")),
                target(2, "sync", "shop", "TASK", false, Some("FAILED")),
            ],
            recent_runs: vec![],
            runs_by_target: HashMap::new(),
            scans: HashMap::new(),
            usage_24h: HashMap::new(),
        }
    }

    #[test]
    fn status_matches_the_dashboard() {
        assert_eq!(status(&target(1, "a", "p", "SERVICE", true, None)).0, "Running (healthy)");
        assert_eq!(status(&target(1, "a", "p", "TASK", false, Some("FAILED"))).0, "Last deploy failed");
        assert_eq!(status(&target(1, "a", "p", "TASK", false, None)).0, "Never deployed");
    }

    #[test]
    fn attention_lists_failures_findings_and_updates() {
        let items = attention(&input(None).targets);
        let text: Vec<&str> = items.iter().map(|(_, t)| t.as_str()).collect();
        assert!(text.iter().any(|t| t.contains("sync: last deploy failed")));
        assert!(text.iter().any(|t| t.contains("2 critical")));
        assert!(text.iter().any(|t| t.contains("2 new commit")));
    }

    #[test]
    fn reports_are_pdfs_without_secrets() {
        for bytes in [overall(&input(None)), project(&input(Some("shop")))] {
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.starts_with("%PDF-1.4") && text.trim_end().ends_with("%%EOF"));
            assert!(!text.contains("hunter2") && !text.contains("tok123"), "no secrets in a report");
        }
        assert!(String::from_utf8_lossy(&project(&input(Some("shop"))))
            .contains("3 variable\\(s\\), values not shown"));
    }

    #[test]
    fn long_tables_break_onto_new_pages() {
        let mut inp = input(None);
        inp.targets =
            (0..120).map(|n| target(n, &format!("t{n}"), "p", "TASK", false, Some("SUCCESS"))).collect();
        let text = String::from_utf8_lossy(&overall(&inp)).into_owned();
        let pages: usize =
            text.split("/Count ").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert!(pages >= 4, "{pages} pages");
    }
}
