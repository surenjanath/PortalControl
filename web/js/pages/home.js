"use strict";
// --------------------------------------------------------------- dashboard --
function targetCard(t) {
  const st = tStatus(t);
  const m = t.metrics;
  return h("a", { class: "box", href: "#/target/" + t.id },
    h("div", { class: "card-top" }, h("span", { class: "nm" }, t.name), h("span", { class: "tag" }, t.run_mode === "SERVICE" ? "Service" : "Task"), h("span", { class: "right" }, pill(st))),
    h("div", { class: "card-desc", title: t.working_directory }, t.description || t.working_directory),
    h("div", { class: "card-meta" }, gitTag(t), secTag(t),
      t.schedule ? h("span", { class: "tag acc", title: "Schedule: " + t.schedule }, icon("clock"), t.next_run ? fmtAgo(new Date(t.next_run * 1000)) : t.schedule) : null,
      t.auto_deploy ? h("span", { class: "tag acc" }, "auto-deploy") : null,
      t.autostart ? h("span", { class: "tag" }, "autostart") : null),
    h("div", { class: "card-foot" },
      t.health_url ? [strip(t, 24), h("span", {}, t.health ? (t.health.status === "DOWN" ? "down" : t.health.latency_ms + " ms") : "checking"),
        t.uptime_pct !== null && t.uptime_pct !== undefined ? h("span", { class: t.uptime_pct < 90 ? "t-err" : t.uptime_pct < 99 ? "t-warn" : "dim", title: "Share of recent health probes that answered" }, t.uptime_pct + "% up") : null] : h("span", { class: "dim" }, "no health probe"),
      m ? h("span", {}, m.cpu.toFixed(1) + "% CPU \u00b7 " + fmtBytes(m.mem)) : null,
      h("span", { class: "right" }, t.running ? "up " + fmtUp(t.running.uptime_secs) : t.last_run ? ["last run ", when(parseUTC(t.last_run.executed_at))] : "never run")));
}
const projHref = (name) => "#/project/" + encodeURIComponent(name);
// Register a detected worker (Django-Q cluster, Celery ...) as a second
// process next to `base`: create its launcher script if needed, then add a
// service target in the same folder and project.
async function addWorker(base, w, project) {
  const made = await api("POST", "/api/fs/launcher", { dir: base.working_directory, kind: w.kind });
  return api("POST", "/api/targets", { name: base.name + " · " + w.label, working_directory: base.working_directory, script_name: made.script,
    git_branch: base.git_branch, run_mode: "SERVICE", project: project || base.project, description: w.label + " for " + base.name,
    auto_restart: true, autostart: !!base.autostart, env_vars: base.env_vars || "" });
}
function byProject(list) {
  const groups = new Map();
  list.forEach((t) => { if (!groups.has(t.project)) groups.set(t.project, []); groups.get(t.project).push(t); });
  return [...groups.entries()].sort((x, y) => x[0].localeCompare(y[0]));
}
// What needs a person, most urgent first: failures, findings, unhealthy
// services, uncommitted changes, updates. The PDF report (report.rs) uses the
// same rules.
function attentionItems(list) {
  const out = [];
  list.forEach((t) => {
    const st = tStatus(t), go = "#/target/" + t.id;
    if (st.cls === "err") out.push({ rank: 0, cls: "err", t, text: st.text.toLowerCase(), href: go + "/logs", action: "Open logs" });
    else if (st.text === "Running (unhealthy)") out.push({ rank: 1, cls: "warn", t, text: "running, but its health check does not answer", href: go + "/metrics", action: "Metrics" });
    if (t.scan && t.scan.critical_issues_found + t.scan.vulnerable_packages_found > 0)
      out.push({ rank: t.scan.critical_issues_found ? 0 : 2, cls: t.scan.critical_issues_found ? "err" : "warn", t,
        text: t.scan.critical_issues_found + " critical issue(s), " + t.scan.vulnerable_packages_found + " vulnerable package(s)", href: go + "/security", action: "Security" });
    if (t.git && t.git.status === "DIRTY") out.push({ rank: 2, cls: "warn", t, text: t.git.dirty_files + " uncommitted change(s) in the working tree", href: go + "/configuration/git", action: "Git" });
    if (t.git && t.git.status === "UPDATES_AVAILABLE") out.push({ rank: 3, cls: "info", t, text: t.git.behind + " new commit(s) on origin, ready to pull", href: go + "/configuration/git", action: "Review & pull" });
    if (t.uptime_pct !== null && t.uptime_pct !== undefined && t.uptime_pct < 90 && t.running) out.push({ rank: 2, cls: "warn", t, text: "answered only " + t.uptime_pct + "% of recent health probes", href: go + "/metrics", action: "Metrics" });
  });
  return out.sort((a, b) => a.rank - b.rank);
}
function attentionPanel(list) {
  const items = attentionItems(list);
  if (!list.length) return null;
  const inc = (sys && sys.open_incidents) || 0;
  const incRow = inc ? h("div", { class: "attn-row" }, h("span", { class: "dot err" }),
    h("span", { class: "grow" }, h("a", { href: "#/incidents", class: "nm" }, inc + " open incident" + (inc === 1 ? "" : "s")), h("span", { class: "muted" }, " \u2014 found by the monitor, with an explanation and a suggested fix")),
    h("a", { class: "btn sm ghost", href: "#/incidents" }, "Review")) : null;
  if (!items.length && !inc) return h("div", { class: "attn clear" }, h("span", { class: "dot ok" }), h("b", {}, "All clear."), h("span", { class: "muted" }, " Everything is running, healthy, up to date and clean."));
  const shown = items.slice(0, 6);
  return h("div", { class: "attn box" },
    h("div", { class: "attn-h" }, h("b", {}, "Needs attention"), h("span", { class: "dim small" }, " " + (items.length + (inc ? 1 : 0)) + " item" + (items.length + (inc ? 1 : 0) === 1 ? "" : "s"))),
    incRow, shown.map((x) => h("div", { class: "attn-row" }, h("span", { class: "dot " + x.cls }),
      h("span", { class: "grow" }, h("a", { href: "#/target/" + x.t.id, class: "nm" }, x.t.name), h("span", { class: "muted" }, " \u2014 " + x.text)),
      h("a", { class: "btn sm ghost", href: x.href }, x.action))),
    items.length > shown.length ? h("div", { class: "dim small", style: "padding:6px 2px 0" }, "+ " + (items.length - shown.length) + " more \u2014 see the PDF report for the full list") : null);
}
const reportLink = (href, label, cls) => h("a", { class: "btn " + (cls || ""), href, download: "", title: "Download a PDF report" }, icon("download"), label);

// A ring split by the state of every target, with a legend.
const DONUT = [["ok", "Running or deployed"], ["info", "Deploying or checking"], ["warn", "Needs a look"], ["err", "Failing"], ["off", "Stopped or never run"]];
function stateDonut(list) {
  const counts = DONUT.map(([c]) => list.filter((t) => tStatus(t).cls === c).length);
  const total = list.length || 1, r = 15.9155; // circumference 100
  const color = (c) => "var(--" + (c === "off" ? "muted" : c) + ")";
  let offset = 25;
  const arcs = counts.map((n, i) => {
    if (!n) return null;
    const len = (n / total) * 100;
    const arc = s("circle", { cx: "21", cy: "21", r: String(r), fill: "none", stroke: color(DONUT[i][0]), "stroke-width": "5", "stroke-dasharray": len + " " + (100 - len), "stroke-dashoffset": String(offset) });
    offset -= len;
    return arc;
  });
  return h("div", {},
    h("div", { class: "chart-h" }, h("b", {}, "Applications by state"), h("span", { class: "dim small" }, "right now")),
    h("div", { class: "donut-row" },
      s("svg", { viewBox: "0 0 42 42", class: "donut", role: "img", "aria-label": DONUT.map(([, label], i) => counts[i] + " " + label.toLowerCase()).join(", ") },
        s("circle", { cx: "21", cy: "21", r: String(r), fill: "none", stroke: "var(--line)", "stroke-width": "5" }), arcs,
        s("text", { x: "21", y: "24", "text-anchor": "middle", class: "donut-n" }, String(list.length))),
      h("div", { class: "donut-legend" }, DONUT.map(([c, label], i) => h("div", { class: counts[i] ? null : "dim" }, h("i", { style: "background:" + color(c) }), label, h("b", {}, counts[i]))))));
}

function dashboardView() {
  let runs = [];
  const def = { key: "dashboard" };
  let c;
  // The last 24 hours of everything, from the analytics (read once a minute).
  let an = null, anAt = 0;
  const loadAn = () => {
    anAt = Date.now();
    api("GET", "/api/analytics?range=24h").then((x) => { an = x; if (view.key === def.key && !c.querySelector(".chart:hover")) paint(); }, () => {});
  };
  const charts = () => {
    if (!targets.length) return null;
    const pts = (k) => (an ? an.series.map((p) => ({ t: p.t, v: p[k] })).filter((p) => p.v !== null && p.v !== undefined) : []);
    const sm = an && an.summary;
    const wait = (text) => (an ? text : "Loading…");
    const o = (extra) => Object.assign({ height: 120, stamp: fmtClock }, extra);
    return [h("h2", {}, "Last 24 hours", h("a", { href: "#/analytics", class: "dim small", style: "font-weight:400;margin-left:10px" }, "more in Analytics")),
      h("div", { class: "grid g2 dash-charts" },
        h("div", { class: "box" }, chartBlock("Availability", sm && sm.availability_pct !== null ? anPct(sm.availability_pct) : "", pts("availability"),
          o({ max: 100, fmt: (v) => Math.round(v) + "%", note: "health probes answered", empty: wait("No health probes recorded"), label: "Share of health probes answered" }))),
        h("div", { class: "box" }, chartBlock("Response time", sm && sm.latency_p50 !== null ? fmtMs(sm.latency_p50) : "", pts("latency"),
          o({ fmt: fmtMs, note: "health checks, averaged", empty: wait("No successful probes"), label: "Health check response time" }))),
        h("div", { class: "box" }, an ? anTimeline(an) : h("p", { class: "muted" }, "Loading…")),
        h("div", { class: "box" }, stateDonut(targets)),
        h("div", { class: "box" }, chartBlock("CPU", "", pts("cpu"), o({ minTop: 1, fmt: fmtPct, note: "all targets, share of all cores", empty: wait("Nothing was running"), label: "CPU of all targets" }))),
        h("div", { class: "box" }, chartBlock("Memory", "", pts("mem"), o({ fmt: fmtBytes, note: "all targets added up", empty: wait("Nothing was running"), label: "Memory of all targets" }))))];
  };
  const paint = () => {
    if (!loaded) return c.replaceChildren(h("p", { class: "muted" }, "Loading\u2026"));
    const running = targets.filter((t) => t.running).length;
    const probed = targets.filter((t) => t.health_url);
    const healthy = probed.filter((t) => t.health && t.health.status !== "DOWN").length;
    const updates = targets.filter((t) => t.git && t.git.is_repo && t.git.behind > 0).length;
    const failed = targets.filter((t) => tStatus(t).cls === "err").length;
    const critical = targets.reduce((n, t) => n + (t.scan ? t.scan.critical_issues_found : 0), 0);
    const vulnerable = targets.reduce((n, t) => n + (t.scan ? t.scan.vulnerable_packages_found : 0), 0);
    const scanned = targets.filter((t) => t.scan).length;
    const hl = sys && sys.host && sys.host.latest;
    const memPct = hl && sys.host.mem_total ? (hl.mem_used / sys.host.mem_total) * 100 : 0;
    c.replaceChildren(
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Dashboard"), h("p", { class: "sub" }, "Deployments, health and Git state of every registered project on this machine.")),
        h("div", { class: "rowf" }, targets.length ? [reportLink("/api/reports/overall.pdf", "Status report"), reportLink("/api/reports/security.pdf", "Security report")] : null,
          h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target"))),
      h("div", { class: "grid g-tiles" },
        tile("Targets", [running, h("small", {}, " / " + targets.length + " running")], targets.filter((t) => t.run_mode === "SERVICE").length + " service(s), " + targets.filter((t) => t.run_mode !== "SERVICE").length + " task(s)"),
        tile("Health", probed.length ? [healthy, h("small", {}, " / " + probed.length + " reachable")] : "-", probed.length ? "probed every 15 s" : "no health URLs configured", probed.length && healthy < probed.length ? "err" : null),
        tile("Updates available", updates, updates ? "new commits on origin" : "all repositories current", updates ? "info" : null),
        tile("Failing", failed, failed ? "last run failed or service exited" : "nothing is failing", failed ? "err" : null),
        tile("Critical issues", scanned ? [critical, h("small", {}, " + " + vulnerable + " pkg")] : "-", scanned ? vulnerable + " vulnerable package(s) \u00b7 " + scanned + " of " + targets.length + " scanned" : "no scans run yet", critical ? "err" : vulnerable ? "warn" : null)),
      attentionPanel(targets),
      hl ? h("div", { class: "grid g-tiles", style: "margin-top:12px" },
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "Host CPU"), h("div", { class: "val" }, hl.cpu.toFixed(0), h("small", {}, " %")), h("div", { class: "bar" }, h("i", { style: "width:" + Math.min(100, hl.cpu) + "%" })), h("div", { class: "note" }, sys.host.cpus + " logical cores")),
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "Host memory"), h("div", { class: "val" }, fmtBytes(hl.mem_used), h("small", {}, " / " + fmtBytes(sys.host.mem_total))), h("div", { class: "bar" }, h("i", { style: "width:" + memPct + "%" })), h("div", { class: "note" }, memPct.toFixed(0) + "% in use")),
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "PortalControl"), h("div", { class: "val" }, fmtBytes(hl.manager_mem)), h("div", { class: "note" }, "manager memory \u00b7 up " + fmtUp(sys.uptime_secs)))) : null,
      charts(),
      targets.length ? byProject(targets).map(([name, list]) => [
        h("h2", { class: "proj-h" }, h("a", { class: "plink", href: projHref(name) }, name), h("span", { class: "dim small", style: "font-weight:400" }, "  " + list.filter((t) => t.running).length + " of " + list.length + " running"),
          list.some((t) => tStatus(t).cls === "err") ? h("span", { class: "tag err", style: "margin-left:8px" }, list.filter((t) => tStatus(t).cls === "err").length + " failing") : null,
          h("a", { class: "dim small", href: projHref(name), style: "font-weight:400;margin-left:10px" }, "terminals"),
          h("a", { class: "dim small", href: "/api/reports/project/" + encodeURIComponent(name), download: "", style: "font-weight:400;margin-left:10px", title: "Download this project's site report (PDF)" }, "site report")),
        h("div", { class: "grid g-cards" }, list.map(targetCard))])
        : [h("h2", {}, "Projects"), empty("No targets yet", "Point PortalControl at a project folder and its deploy script to get started.", h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target"))],
      h("div", { class: "grid g2", style: "margin-top:8px" },
        h("div", {}, h("h2", {}, "Recent deployments", h("a", { href: "#/deployments", class: "dim small", style: "font-weight:400;margin-left:10px" }, "view all")),
          runs.length ? runs.slice(0, 6).map((r) => depRow(r, true)) : h("div", { class: "box muted" }, "Nothing has been deployed yet.")),
        h("div", {}, h("h2", {}, "Activity", h("a", { href: "#/activity", class: "dim small", style: "font-weight:400;margin-left:10px" }, "view all")),
          h("div", { class: "box" }, activity.length ? activity.slice(0, 8).map((e) => feedRow(e, true)) : h("span", { class: "muted" }, "No activity since the manager started.")))));
  };
  def.build = () => { c = h("div"); paint(); def.update(); return c; };
  def.update = () => {
    // A chart under the pointer is not redrawn, so its tooltip stays.
    if (!c.querySelector(".chart:hover")) paint();
    if (Date.now() - anAt > 60000) loadAn();
    api("GET", "/api/runs?limit=6").then((x) => { runs = x; if (view.key === def.key && !c.querySelector(".chart:hover")) paint(); }, () => {});
  };
  return def;
}

// ---------------------------------------------------------------- projects --
function projectsView() {
  const def = { key: "projects" };
  def.build = () => {
    const body = h("div");
    const search = h("input", { type: "search", placeholder: "Filter by name, path or project\u2026", value: ui.projFilter, style: "max-width:340px", "aria-label": "Filter targets",
      oninput: () => { ui.projFilter = search.value; def.update(); } });
    def.update = () => {
      const q = ui.projFilter.trim().toLowerCase();
      const list = targets.filter((t) => !q || (t.name + " " + t.working_directory + " " + t.project + " " + t.description).toLowerCase().includes(q));
      body.replaceChildren(...(!targets.length ? [empty("No targets yet", "Register a project folder and the script that deploys or starts it.", h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target"))]
        : !list.length ? [empty("No matches", "No target matches \u201c" + ui.projFilter + "\u201d.")]
        : byProject(list).flatMap(([name, items]) => [h("h2", {}, h("a", { class: "plink", href: projHref(name) }, name),
            h("a", { class: "dim small", href: projHref(name), style: "font-weight:400;margin-left:10px" }, "processes and terminals")), table(
            ["Target", "Status", "Git", "Health", "Last deployment", ""],
            items.map((t) => [
              h("div", {}, h("a", { href: "#/target/" + t.id, style: "font-weight:600" }, t.name), " ", h("span", { class: "tag" }, t.run_mode === "SERVICE" ? "Service" : "Task"),
                h("div", { class: "dim mono", style: "font-size:11.5px;margin-top:2px;word-break:break-all" }, t.working_directory + " \u279c " + t.script_name)),
              pill(tStatus(t)),
              h("div", {}, gitTag(t) || h("span", { class: "dim" }, "not a repo"), t.git && t.git.is_repo ? h("div", { class: "dim small mono", style: "margin-top:3px" }, short(t.git.head) + " " + (t.git.checked_out_branch || "detached")) : null),
              t.health_url ? h("div", {}, strip(t, 20), h("div", { class: "dim small" }, t.uptime_pct !== null ? t.uptime_pct + "% up" : "")) : h("span", { class: "dim" }, "-"),
              t.last_run ? h("div", {}, runTag(t.last_run.status), h("div", { class: "dim small", style: "margin-top:3px" }, when(parseUTC(t.last_run.executed_at)), " \u00b7 ", fmtDur(t.last_run.duration_ms))) : h("span", { class: "dim" }, "never"),
              h("div", { class: "rowf", style: "justify-content:flex-end;flex-wrap:nowrap" }, actionButtons(t, true)),
            ]))])));
    };
    def.update();
    return h("div", {},
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Projects"), h("p", { class: "sub" }, "All registered targets, grouped by project.")),
        h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target")),
      h("div", { style: "margin-bottom:6px" }, search), body);
  };
  return def;
}

