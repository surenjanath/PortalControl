"use strict";
// --------------------------------------------------------------- analytics --
// One page over everything that is stored (GET /api/analytics, analytics.rs):
// figures for the whole machine, trends, when runs fail, a table that compares
// the targets, a reliability score with its reasons, and findings worked out
// from the numbers (growing memory, slower answers, outages, noisy logs).

const AN_RANGES = [["24h", "24 hours"], ["7d", "7 days"], ["30d", "30 days"]];
const AN_KINDS = { failure: "Failed run", crash_loop: "Crash loop", health_down: "Health check down", error_burst: "Burst of errors", new_error: "New error",
  attack: "Attack", brute_force: "Password guessing", flood: "Flood of requests" };
const AN_DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const anNone = (v) => v === null || v === undefined;
function anCount(n) {
  if (anNone(n)) return "-";
  if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + "M";
  if (n >= 1e4) return Math.round(n / 1e3) + "k";
  return String(n);
}
function anMinutes(m) {
  if (anNone(m)) return "-";
  m = Math.round(m);
  if (m < 60) return m + " min";
  if (m < 1440) return Math.floor(m / 60) + "h " + (m % 60) + "m";
  return Math.floor(m / 1440) + "d " + Math.floor((m % 1440) / 60) + "h";
}
const anPct = (v) => (anNone(v) ? "-" : (v >= 99.995 ? "100" : v.toFixed(v < 99 ? 1 : 2)) + "%");
const anScoreCls = (v) => (v >= 90 ? "ok" : v >= 70 ? "warn" : "err");
function anScore(t) {
  const why = t.deductions.length ? t.deductions.map((d) => "−" + d.points + "  " + d.what).join("\n") : "No points lost in this period";
  return h("span", { class: "tag " + anScoreCls(t.score), title: why }, t.score);
}
const anDay = (d) => pad(d.getMonth() + 1) + "-" + pad(d.getDate());

// Runs per bar, succeeded under failed, and the incidents in a row of their own.
function anTimeline(d) {
  const hourly = d.step_secs < 86400;
  const label = (x) => { const at = new Date(x.t * 1000); return hourly ? anDay(at) + " " + pad(at.getHours()) + ":00" : anDay(at); };
  const topRuns = Math.max(1, ...d.timeline.map((x) => x.ok + x.failed));
  const topInc = Math.max(1, ...d.timeline.map((x) => x.incidents));
  const seg = (cls, n, top) => (n ? h("i", { class: cls, style: "height:" + Math.max(4, (n / top) * 100) + "%" }) : null);
  const first = d.timeline[0], last = d.timeline[d.timeline.length - 1];
  const axis = () => h("div", { class: "an-x" }, h("span", {}, first ? label(first) : ""), h("span", {}, last ? label(last) : ""));
  return h("div", {},
    h("div", { class: "chart-h" }, h("b", {}, "Runs"), h("span", { class: "dim small" }, hourly ? "per hour" : "per day"), h("span", { class: "now" }, "most " + topRuns)),
    h("div", { class: "an-cols" }, d.timeline.map((x) => h("div", { class: "an-col", title: label(x) + ": " + x.ok + " succeeded, " + x.failed + " failed" },
      x.ok + x.failed ? [seg("ok", x.ok, topRuns), seg("bad", x.failed, topRuns)] : h("i", { class: "zero" })))),
    axis(),
    h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--ok)" }), "Succeeded"), h("span", {}, h("i", { style: "background:var(--err)" }), "Failed")),
    h("div", { class: "chart-h", style: "margin-top:18px" }, h("b", {}, "Incidents"), h("span", { class: "dim small" }, hourly ? "opened per hour" : "opened per day"), h("span", { class: "now" }, "most " + topInc)),
    h("div", { class: "an-cols", style: "height:60px" }, d.timeline.map((x) => h("div", { class: "an-col", title: label(x) + ": " + x.incidents + " incident" + (x.incidents === 1 ? "" : "s") },
      x.incidents ? seg("inc", x.incidents, topInc) : h("i", { class: "zero" })))),
    axis());
}

// Failed runs and incidents by weekday and hour: one hue, darker for more.
function anHeatmap(d) {
  const top = Math.max(0, ...d.heatmap.flat());
  const cells = [h("span")];
  for (let hr = 0; hr < 24; hr++) cells.push(h("span", { class: "hh" }, hr % 3 === 0 ? pad(hr) : ""));
  d.heatmap.forEach((row, day) => {
    cells.push(h("span", {}, AN_DAYS[day]));
    row.forEach((n, hr) => cells.push(h("i", { class: n ? "on" : null, style: n ? "opacity:" + (0.25 + 0.75 * (n / top)).toFixed(2) : null,
      title: AN_DAYS[day] + " " + pad(hr) + ":00–" + pad(hr) + ":59: " + n + " failed run" + (n === 1 ? "" : "s") + " and incidents" })));
  });
  return h("div", {},
    h("div", { class: "chart-h" }, h("b", {}, "When things go wrong"), h("span", { class: "dim small" }, "failed runs and incidents by weekday and hour")),
    top ? [h("div", { class: "an-heat", style: "margin-top:10px" }, cells),
      h("div", { class: "legend" }, h("span", {}, "fewer", [0.25, 0.5, 0.75, 1].map((o) => h("i", { style: "background:var(--series);opacity:" + o })), "more"), h("span", { class: "right" }, "busiest hour: " + top))]
      : h("div", { class: "muted small", style: "padding:24px 0" }, "No failed runs or incidents in this period."));
}

// Horizontal bars of two parts each. rows: [{key, a, b}].
function anBars(title, note, rows, name, partA, partB, clsA, clsB) {
  const top = Math.max(1, ...rows.map((r) => r.a + r.b));
  return h("div", {},
    h("div", { class: "chart-h" }, h("b", {}, title), h("span", { class: "dim small" }, note)),
    rows.length ? [h("div", { style: "margin-top:8px" }, rows.map((r) => h("div", { class: "an-hbar", title: name(r.key) + ": " + r.a + " " + partA + ", " + r.b + " " + partB },
      h("span", {}, name(r.key)),
      h("span", { class: "trk" }, r.a ? h("i", { style: "width:" + (r.a / top) * 100 + "%;background:var(--" + clsA + ")" }) : null, r.b ? h("i", { style: "width:" + (r.b / top) * 100 + "%;background:var(--" + clsB + ")" }) : null),
      h("span", { class: "dim small" }, r.a + r.b)))),
      h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--" + clsA + ")" }), partA), h("span", {}, h("i", { style: "background:var(--" + clsB + ")" }), partB))]
      : h("div", { class: "muted small", style: "padding:24px 0" }, "Nothing in this period."));
}

function anFindings(d) {
  if (!d.insights.length) return h("div", { class: "attn clear", style: "margin:0 0 18px" }, h("span", { class: "dot ok" }), h("b", {}, "Nothing stands out."), h("span", { class: "muted" }, " No outages, crash loops, growing memory or unusual response times in this period."));
  return h("div", { class: "box an-find", style: "margin-bottom:18px" },
    h("div", { class: "attn-h" }, h("b", {}, "Findings"), h("span", { class: "dim small" }, " " + d.insights.length + " worked out from the figures below, most serious first")),
    d.insights.map((x) => h("div", { class: "attn-row" }, h("span", { class: "dot " + x.level }),
      h("span", { class: "grow" }, x.target_id ? [h("a", { href: "#/target/" + x.target_id + "/metrics", class: "nm" }, x.target), " "] : null, h("span", { class: "muted" }, x.text)),
      x.target_id ? h("a", { class: "btn sm ghost", href: "#/target/" + x.target_id + "/metrics" }, "Open") : null)));
}

function anTargetTable(d, sort, onSort) {
  const pair = (a, b) => (anNone(a) ? "-" : [a, h("span", { class: "dim" }, " / " + b)]);
  const cols = [
    { k: "name", label: "Target", get: (t) => t.name.toLowerCase(), cell: (t) => [h("a", { href: "#/target/" + t.id + "/metrics", style: "font-weight:600" }, t.name), h("div", { class: "dim small" }, t.project)] },
    { k: "score", label: "Score", num: true, tip: "100 minus points for missed health probes, failed runs, crash restarts, incidents and security findings. Hover a score for its reasons.", cell: anScore },
    { k: "availability_pct", label: "Availability", num: true, cell: (t) => (t.has_health || !anNone(t.availability_pct) ? anPct(t.availability_pct) : h("span", { class: "dim", title: "No health URL" }, "-")) },
    { k: "latency_p50", label: "Response", num: true, tip: "Typical (median) response time of the health check / 95% answered within", cell: (t) => pair(anNone(t.latency_p50) ? null : fmtMs(t.latency_p50), anNone(t.latency_p95) ? "" : fmtMs(t.latency_p95)) },
    { k: "downtime_minutes", label: "Unreachable", num: true, tip: "Time in which every health probe failed, and the number of outages", cell: (t) => (t.outages ? [anMinutes(t.downtime_minutes), h("span", { class: "dim" }, " in " + t.outages)] : h("span", { class: "dim" }, "none")) },
    { k: "mttr_minutes", label: "Recovery", num: true, tip: "Average time to come back after an outage (MTTR). Hover a value for the average time between outages (MTBF).", cell: (t) => (anNone(t.mttr_minutes) ? "-" : h("span", { title: "On average " + anMinutes(t.mtbf_minutes) + " between outages" }, anMinutes(t.mttr_minutes))) },
    { k: "cpu_avg", label: "CPU", num: true, tip: "Average / peak, as a share of all cores, while running", cell: (t) => pair(anNone(t.cpu_avg) ? null : fmtPct(t.cpu_avg), anNone(t.cpu_peak) ? "" : fmtPct(t.cpu_peak)) },
    { k: "mem_avg", label: "Memory", num: true, tip: "Average / peak of the whole process tree", cell: (t) => pair(anNone(t.mem_avg) ? null : fmtBytes(t.mem_avg), fmtBytes(t.mem_peak)) },
    { k: "mem_trend_per_hour", label: "Memory trend", num: true, tip: "Slope of a straight line through the memory minutes. Marked when the rise is steady and large.",
      cell: (t) => (anNone(t.mem_trend_per_hour) ? "-" : Math.abs(t.mem_trend_per_hour) < 1048576 ? h("span", { class: "dim", title: "Less than 1 MB per hour" }, "flat") : h("span", { class: t.mem_growing ? "tag warn" : null, title: "Fit " + Math.round((t.mem_trend_fit || 0) * 100) + "%" + (t.mem_growing ? " — steady rise, possible leak" : "") },
        (t.mem_trend_per_hour < 0 ? "−" : "+") + fmtBytes(Math.abs(t.mem_trend_per_hour)) + "/h")) },
    { k: "runs_failed", label: "Runs", num: true, tip: "Failed / all runs of the script (quick commands are not counted)", cell: (t) => (t.runs ? [h("span", { class: t.runs_failed ? "t-err" : null }, t.runs_failed), h("span", { class: "dim" }, " / " + t.runs)] : h("span", { class: "dim" }, "none")) },
    { k: "run_avg_ms", label: "Run time", num: true, tip: "Average duration / 95% finish within", cell: (t) => pair(anNone(t.run_avg_ms) ? null : fmtDur(t.run_avg_ms), fmtDur(t.run_p95_ms)) },
    { k: "crash_restarts", label: "Crashes", num: true, tip: "Automatic restarts after a crash", cell: (t) => (t.crash_restarts ? h("span", { class: "t-warn" }, t.crash_restarts) : h("span", { class: "dim" }, "0")) },
    { k: "incidents", label: "Incidents", num: true, cell: (t) => (t.incidents ? [t.incidents, t.incidents_open ? h("span", { class: "t-err" }, " (" + t.incidents_open + " open)") : null] : h("span", { class: "dim" }, "0")) },
    { k: "lines", label: "Log lines", num: true, tip: "Lines read since the manager started, and the share that look like errors", cell: (t) => (t.lines ? [anCount(t.lines), h("span", { class: "dim" }, " · " + Math.round((t.error_lines / t.lines) * 100) + "% err")] : h("span", { class: "dim" }, "0")) },
  ];
  const col = cols.find((c) => c.k === sort.col) || cols[1];
  const value = (t) => (col.get ? col.get(t) : t[col.k]);
  const rows = d.targets.slice().sort((x, y) => {
    const a = value(x), b = value(y);
    if (anNone(a) || anNone(b)) return anNone(a) - anNone(b);   // no figure: always last
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir || x.name.localeCompare(y.name);
  });
  return h("div", { class: "tblwrap" }, h("table", { class: "tbl" },
    h("thead", {}, h("tr", {}, cols.map((c) => h("th", { class: "sort" + (c.num ? " num" : ""), title: (c.tip ? c.tip + "\n" : "") + "Click to sort", onclick: () => onSort(c.k, c.num),
      "aria-sort": c.k === col.k ? (sort.dir > 0 ? "ascending" : "descending") : null }, c.label, c.k === col.k ? (sort.dir > 0 ? " ↑" : " ↓") : "")))),
    h("tbody", {}, rows.map((t) => h("tr", {}, cols.map((c) => h("td", { class: c.num ? "num" : null }, c.cell(t))))))));
}

function analyticsView() {
  const def = { key: "analytics" };
  def.build = () => {
    const body = h("div"), switcher = h("div", { class: "termtabs", style: "margin:0" });
    const sort = ui.anSort || (ui.anSort = { col: "score", dir: 1 });
    let range = ui.anRange || "7d", data = null, error = null, seq = 0, loadedAt = 0;
    const paintSwitch = () => switcher.replaceChildren(AN_RANGES.map(([k, label]) => btn(label, () => { range = ui.anRange = k; data = null; paintSwitch(); paint(); load(); }, "sm" + (k === range ? " on" : ""))));
    const onSort = (k, num) => { if (sort.col === k) sort.dir = -sort.dir; else { sort.col = k; sort.dir = num && k !== "score" ? -1 : 1; } paint(); };

    function paint() {
      if (error && !data) return body.replaceChildren(h("div", { class: "note-box err" }, "The figures could not be loaded: " + error));
      if (!data) return body.replaceChildren(h("p", { class: "muted" }, "Loading…"));
      const d = data, sm = d.summary, o = { stamp: fmtStamp };
      if (!sm.targets) return body.replaceChildren(empty("No targets yet", "Analytics are worked out from the runs, health probes and output of registered targets.", h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target")));
      const pts = (k) => d.series.map((p) => ({ t: p.t, v: p[k] })).filter((p) => !anNone(p.v));
      const per = "average per " + d.bucket_secs / 60 + " min";
      const span = AN_RANGES.find(([k]) => k === d.range)[1];
      const shortMetrics = d.range_secs > d.metrics_kept_secs;
      const ai = d.ai, mon = d.monitor;
      body.replaceChildren(
        shortMetrics ? h("div", { class: "note-box" }, "Runs and incidents cover the last " + span + ". Availability, response time, CPU and memory are stored per minute for 7 days, so those figures cover the last 7 days. The history keeps the newest 200 runs per target and 500 incidents.") : null,
        h("div", { class: "grid g-tiles", style: "margin-bottom:18px" },
          tile("Reliability score", anNone(sm.score) ? "-" : [sm.score, h("small", {}, " / 100")], "average of " + sm.targets + " target" + (sm.targets === 1 ? "" : "s"), anNone(sm.score) || sm.score >= 90 ? null : anScoreCls(sm.score)),
          tile("Availability", anPct(sm.availability_pct), sm.outages ? sm.outages + " outage" + (sm.outages === 1 ? "" : "s") + ", " + anMinutes(sm.downtime_minutes) + " unreachable" : anNone(sm.availability_pct) ? "no health probes recorded" : "no outages", sm.outages ? (sm.availability_pct < 95 ? "err" : "warn") : null),
          tile("Response time", anNone(sm.latency_p50) ? "-" : fmtMs(sm.latency_p50), anNone(sm.latency_p95) ? "no successful probes" : "typical · 95% under " + fmtMs(sm.latency_p95)),
          tile("Runs succeeded", anNone(sm.success_pct) ? "-" : [sm.success_pct, h("small", {}, " %")], sm.runs ? sm.runs_ok + " succeeded, " + sm.runs_failed + " failed" : "no runs in this period", sm.runs && sm.success_pct < 80 ? "warn" : null),
          tile("Crash restarts", sm.crash_restarts, sm.crash_restarts ? "automatic restarts after a crash" : "none", sm.crash_restarts ? "warn" : null),
          tile("Incidents", sm.incidents, sm.incidents_open ? sm.incidents_open + " open now" : "none open", sm.incidents_open ? "err" : null),
          tile("Log lines", anCount(mon.lines), mon.lines ? Math.round((mon.error_lines / mon.lines) * 100) + "% look like errors · since start" : "since the manager started"),
          tile("Security findings", sm.scanned ? [sm.critical_issues, h("small", {}, " + " + sm.vulnerable_packages + " pkg")] : "-", sm.scanned ? "critical issues + vulnerable packages · " + sm.scanned + " of " + sm.targets + " scanned" : "no scans run yet", sm.critical_issues ? "err" : sm.vulnerable_packages ? "warn" : null)),
        anFindings(d),

        h("h2", {}, "Trends", h("span", { class: "dim small", style: "font-weight:400" }, "  all targets together")),
        d.series.length ? h("div", { class: "grid g2" },
          h("div", { class: "box" }, chartBlock("Availability", anPct(sm.availability_pct), pts("availability"), Object.assign({ max: 100, fmt: (v) => Math.round(v) + "%", note: "health probes answered", empty: "No health probes recorded", label: "Share of health probes answered, all targets" }, o))),
          h("div", { class: "box" }, chartBlock("Response time", anNone(sm.latency_p50) ? "" : fmtMs(sm.latency_p50), pts("latency"), Object.assign({ fmt: fmtMs, note: per, empty: "No successful probes", label: "Health response time, averaged over the targets" }, o))),
          h("div", { class: "box" }, chartBlock("CPU", "", pts("cpu"), Object.assign({ minTop: 1, fmt: fmtPct, note: "all targets added up, share of all cores", empty: "Nothing was running", label: "CPU of all targets" }, o))),
          h("div", { class: "box" }, chartBlock("Memory", "", pts("mem"), Object.assign({ fmt: fmtBytes, note: "all targets added up", empty: "Nothing was running", label: "Memory of all targets" }, o))),
          h("div", { class: "box" }, chartBlock("Targets running", "", pts("running"), Object.assign({ minTop: Math.max(2, sm.targets), fmt: (v) => String(Math.round(v)), note: "of " + sm.targets + " registered", label: "Number of targets running" }, o))))
          : h("div", { class: "note-box" }, "Nothing has been recorded for this period yet. One row per minute is stored while a target is running or has a health URL."),

        h("h2", {}, "Runs and incidents"),
        h("div", { class: "box", style: "margin-bottom:12px" }, anTimeline(d)),
        h("div", { class: "grid g2" },
          h("div", { class: "box" }, anHeatmap(d)),
          h("div", { class: "box" }, anBars("What started the runs", "", d.runs_by_trigger, trig, "succeeded", "failed", "ok", "err")),
          h("div", { class: "box" }, anBars("Incidents by kind", "what the monitor saw", d.incidents_by_kind, (k) => AN_KINDS[k] || k, "closed", "open", "dim", "err")),
          h("div", { class: "box" }, anBars("Incidents by severity", "", d.incidents_by_severity, (k) => k.charAt(0).toUpperCase() + k.slice(1), "closed", "open", "dim", "err"))),

        h("h2", {}, "Targets compared", h("span", { class: "dim small", style: "font-weight:400" }, "  click a column to sort; hover a score to see what cost points")),
        anTargetTable(d, sort, onSort),

        d.projects.length > 1 ? [h("h2", {}, "Projects"),
          table(["Project", "Targets", "Score", "Availability", "Unreachable", "Runs (failed)", "Crashes", "Incidents", "Log lines"], d.projects.map((p) => [
            h("a", { href: projHref(p.name), style: "font-weight:600" }, p.name), p.targets, anNone(p.score) ? "-" : h("span", { class: "tag " + anScoreCls(p.score) }, p.score), anPct(p.availability_pct),
            p.downtime_minutes ? anMinutes(p.downtime_minutes) : h("span", { class: "dim" }, "none"), p.runs ? p.runs + " (" + p.runs_failed + ")" : h("span", { class: "dim" }, "none"), p.crash_restarts, p.incidents, anCount(p.lines)]))] : null,

        h("h2", {}, "Monitor and AI", h("a", { href: "#/incidents", class: "dim small", style: "font-weight:400;margin-left:10px" }, "live view")),
        h("div", { class: "grid g-tiles" },
          tile("Lines read", anCount(mon.lines), anCount(mon.error_lines) + " looked like errors · since start"),
          tile("Rule hits", mon.flagged, "opened an incident · since start"),
          tile("Requests read", anCount(mon.requests), mon.threats + " attack rule hit" + (mon.threats === 1 ? "" : "s") + " · since start", mon.threats ? "warn" : null),
          tile("AI analyses", ai.answered, ai.failed ? ai.failed + " failed · " + ai.rules_only + " by rules only" : ai.rules_only + " incident" + (ai.rules_only === 1 ? "" : "s") + " by rules only", ai.failed ? "warn" : null),
          tile("AI answer time", anNone(ai.avg_ms) ? "-" : fmtDur(ai.avg_ms), "average per analysis"),
          tile("AI tokens", anCount(ai.prompt_tokens + ai.answer_tokens), anCount(ai.prompt_tokens) + " read, " + anCount(ai.answer_tokens) + " written"),
          tile("Alert emails", mon.emails, "sent since start")),
        h("p", { class: "dim small", style: "margin-top:14px" }, "Worked out " + fmtAgo(new Date(d.generated_at * 1000)) + " from " + fmtDT(new Date(d.since * 1000)) + " on. Days and hours are in this computer's time zone. Figures marked “since start” are counted in memory and begin again when PortalControl restarts."));
    }
    function load() {
      const mine = ++seq;
      loadedAt = Date.now();
      api("GET", "/api/analytics?range=" + range).then((x) => { if (mine !== seq) return; data = x; error = null; if (!body.querySelector(".chart:hover")) paint(); },
        (e) => { if (mine !== seq) return; error = e.message; paint(); });
    }
    // The page is refreshed by every event; the figures are read once a minute.
    def.update = () => { if (Date.now() - loadedAt > 60000) load(); };
    paintSwitch(); paint(); load();
    return h("div", {},
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Analytics"), h("p", { class: "sub" }, "Reliability, performance, runs, incidents and logs of every target, with what stands out.")),
        h("div", { class: "rowf" }, switcher, btn("Refresh", load, "sm", "restart", { title: "Work the figures out again now" }))),
      body);
  };
  return def;
}
