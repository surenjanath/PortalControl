"use strict";
// ---- deployments ----
function depsBody(id) {
  const c = h("div");
  let runs = null;
  const paint = () => {
    if (!runs) return c.replaceChildren(h("p", { class: "muted" }, "Loading\u2026"));
    const done = runs.filter((r) => r.status !== "RUNNING");
    const okN = done.filter((r) => r.status === "SUCCESS").length;
    const avg = done.length ? done.reduce((n, r) => n + (r.duration_ms || 0), 0) / done.length : null;
    c.replaceChildren(...(runs.length ? [
      h("div", { class: "grid g-tiles", style: "margin-bottom:18px" },
        tile("Runs", runs.length, "most recent 100 are listed"), tile("Success rate", done.length ? [Math.round((okN / done.length) * 100), h("small", {}, " %")] : "-", okN + " succeeded, " + (done.length - okN) + " failed"),
        tile("Average duration", fmtDur(avg === null ? null : Math.round(avg))), tile("Last run", when(parseUTC(runs[0].executed_at)), trig(runs[0].triggered_by))),
      runs.map((r) => depRow(r, false))]
      : [empty("No deployments yet", "Run the script to create the first one.", actionButtons(T(id) || {}))]));
  };
  const load = () => api("GET", "/api/targets/" + id + "/history").then((x) => { runs = x; paint(); }, () => {});
  load();
  return { el: c, update: load };
}
function runBody(id, runId) {
  const meta = h("div");
  const log = LogView("run-" + runId);
  let run = null, lastStatus = null;
  const paintMeta = () => {
    if (!run) return;
    meta.replaceChildren(
      h("div", { class: "crumbs" }, h("a", { href: "#/target/" + id + "/deployments" }, "\u2190 All deployments")),
      h("div", { class: "box", style: "margin-bottom:16px" }, h("div", { class: "rowf", style: "margin-bottom:10px" }, h("b", { style: "font-size:16px" }, run.label ? run.label + " (#" + run.id + ")" : "Deployment #" + run.id), runTag(run.status),
          run.status === "RUNNING" && run.label ? btn("Stop command", () => act(() => api("POST", "/api/targets/" + id + "/commands/stop"), "Stopping…"), "danger sm right", "stop") : null),
        facts([["Started", [fmtDT(parseUTC(run.executed_at)), h("span", { class: "dim" }, "  (" + fmtAgo(parseUTC(run.executed_at)) + ")")]],
          ["Triggered by", trig(run.triggered_by)], ["Duration", run.status === "RUNNING" ? "in progress" : fmtDur(run.duration_ms)],
          ["Exit code", run.exit_code === null ? (run.status === "RUNNING" ? "-" : "none (stopped or timed out)") : run.exit_code],
          ["Commit", run.commit_sha ? h("span", { class: "rowf" }, h("span", {}, h("span", { class: "mono" }, short(run.commit_sha)), " ", run.commit_message || ""), rollbackButton(T(id), run.commit_sha, true)) : "not recorded"]])));
  };
  const load = (withLogs) => api("GET", "/api/runs/" + runId).then((r) => {
    run = r; paintMeta();
    if (withLogs || (lastStatus === "RUNNING" && r.status !== "RUNNING")) log.setText(r.captured_logs || "(no output captured)");
    lastStatus = r.status;
  }, (e) => meta.replaceChildren(empty("Deployment not found", e.message)));
  load(true);
  const sink = (m) => {
    if (m.type === "log" && m.run_id === Number(runId)) log.add(m.line, m.stream, m.ts);
    else if (m.type === "run_finished" && m.run_id === Number(runId)) load(false);
  };
  sinks.add(sink);
  return { el: h("div", {}, meta, log.el), update: () => { if (run && run.status === "RUNNING") load(false); else paintMeta(); }, cleanup: () => sinks.delete(sink) };
}

// ---- quick commands ----
function commandsBody(id) {
  const c = h("div");
  let data = null;
  const run = async (cmd) => {
    const t = T(id);
    if (cmd.changes && !(await confirmDlg(cmd.label + "?", [h("p", {}, cmd.description), h("p", { class: "mono" }, cmd.command),
      cmd.backup_first ? h("p", {}, "The database is backed up first.") : (cmd.id === "migrate" && t && !t.backup_on_deploy ? h("p", {}, "Tip: enable Database Backups to snapshot the database automatically before this runs.") : null)], "Run"))) return;
    try { const r = await api("POST", "/api/targets/" + id + "/commands/" + cmd.id); toast(cmd.label + " started"); await refresh(); go("target/" + id + "/deployments/" + r.run_id); }
    catch (e) { toast(e.message, true); }
  };
  const paint = () => {
    const t = T(id);
    if (!data || !t) return c.replaceChildren(h("p", { class: "muted" }, "Loading…"));
    const busy = !!t.command_running;
    c.replaceChildren(
      h("div", { class: "note-box" }, "One-click maintenance commands for this project. Each runs in the working directory with the target's environment variables, streams to the log like a deployment, and is recorded under Deployments. They run next to a live service without stopping it. Only this fixed list can be run; nothing typed here is executed.",
        h("div", { style: "margin-top:6px" }, "Python: ", h("span", { class: "mono" }, data.python))),
      busy ? h("div", { class: "note-box ok rowf" }, "A command is running.", h("a", { class: "btn sm", href: "#/target/" + id + "/deployments/" + t.command_running.run_id }, "Watch output"),
        btn("Stop", () => act(() => api("POST", "/api/targets/" + id + "/commands/stop"), "Stopping…"), "danger sm", "stop")) : null,
      data.commands.length ? h("div", { class: "grid g-cards" }, data.commands.map((cmd) => h("div", { class: "box" },
        h("div", { class: "card-top" }, h("span", { class: "nm" }, cmd.label), cmd.changes ? h("span", { class: "tag warn" }, "makes changes") : h("span", { class: "tag" }, "read-only")),
        h("div", { class: "muted small", style: "margin:6px 0 8px;min-height:38px" }, cmd.description),
        h("div", { class: "mono dim", style: "font-size:11.5px;word-break:break-all;margin-bottom:12px" }, cmd.command),
        btn("Run", () => run(cmd), cmd.changes ? "" : "hl", "play", { disabled: busy }))))
        : empty("No commands for this project type", "Quick commands are offered for Django projects (manage.py), Python projects with a requirements.txt and Node projects with a package.json."));
  };
  api("GET", "/api/targets/" + id + "/commands").then((d) => { data = d; paint(); }, (e) => c.replaceChildren(empty("Could not load commands", e.message)));
  return { el: c, update: paint };
}

// ---- logs ----
function logsBody(id) {
  const t = T(id);
  const log = LogView(t.name.replace(/[^\w.-]+/g, "_"));
  const info = h("div", { class: "muted small", style: "margin-bottom:10px" });
  api("GET", "/api/targets/" + id + "/logs").then((d) => {
    info.textContent = d.run_id ? (d.live ? "Live output of run #" + d.run_id + "." : "Output of the last run (#" + d.run_id + ", " + String(d.status).toLowerCase() + "). New output appears here as soon as the target runs again.") : "This target has not run yet. Output will stream here.";
    log.setText(d.logs);
  }, (e) => log.setText("failed to load logs: " + e.message));
  const sink = (m) => {
    if (m.target_id !== id) return;
    if (m.type === "log") log.add(m.line, m.stream, m.ts);
    else if (m.type === "run_started") { log.add("\u2500\u2500\u2500\u2500 run #" + m.run_id + " started \u2500\u2500\u2500\u2500", "sep", Date.now()); info.textContent = "Live output of run #" + m.run_id + "."; }
    else if (m.type === "lagged") log.add("[portalcontrol] " + m.skipped + " line(s) skipped (slow connection)", "system");
  };
  sinks.add(sink);
  return { el: h("div", {}, info, log.el), cleanup: () => sinks.delete(sink) };
}

// ---- metrics ----
function metricsBody(id) {
  const c = h("div");
  let data = null;
  const paint = () => {
    const t = T(id);
    if (!data || !t) return c.replaceChildren(h("p", { class: "muted" }, "Loading\u2026"));
    const [m, hh, runs] = data;
    const last = m.samples[m.samples.length - 1];
    const lat = hh.samples.filter((x) => x.latency_ms !== null);
    const recent = runs.slice(0, 40).reverse();
    const maxDur = Math.max(1, ...recent.map((r) => r.duration_ms || 0));
    c.replaceChildren(
      !t.running && !m.samples.length ? h("div", { class: "note-box" }, "CPU and memory are sampled every 5 seconds while the target is running, across its whole process tree. Start it to see live figures.") : null,
      h("div", { class: "grid g2" },
        h("div", { class: "box" }, chartBlock("CPU", last && t.running ? last.cpu.toFixed(1) + " %" : "", m.samples.map((x) => ({ t: x.t, v: x.cpu })),
          { minTop: 1, fmt: (v) => (v < 10 ? v.toFixed(1) : Math.round(v)) + "%", note: "of all " + m.cpus + " cores", empty: "Not running", label: "CPU usage of the process tree" })),
        h("div", { class: "box" }, chartBlock("Memory", last && t.running ? fmtBytes(last.mem) : "", m.samples.map((x) => ({ t: x.t, v: x.mem })),
          { fmt: fmtBytes, note: last ? last.procs + " process(es) in the tree" : "", empty: "Not running", label: "Memory of the process tree" })),
        t.health_url ? h("div", { class: "box" }, chartBlock("Health response time", lat.length ? lat[lat.length - 1].latency_ms + " ms" : "", lat.map((x) => ({ t: x.t, v: x.latency_ms })),
          { fmt: (v) => Math.round(v) + " ms", note: t.uptime_pct !== null ? t.uptime_pct + "% reachable" : "", empty: "No successful probes yet", label: "Health probe response time" })) : null,
        h("div", { class: "box" }, h("div", { class: "chart-h" }, h("b", {}, "Run durations"), h("span", { class: "dim small" }, "last " + recent.length + " runs, oldest first")),
          recent.length ? [h("div", { class: "bars" }, recent.map((r) => h("i", { class: r.status, style: "height:" + Math.max(4, ((r.duration_ms || 0) / maxDur) * 100) + "%",
              title: "#" + r.id + " " + r.status.toLowerCase() + " \u00b7 " + fmtDur(r.duration_ms) + " \u00b7 " + fmtDT(parseUTC(r.executed_at)), onclick: () => go("target/" + id + "/deployments/" + r.id) }))),
            h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--ok)" }), "Success"), h("span", {}, h("i", { style: "background:var(--err)" }), "Failed"), h("span", {}, h("i", { style: "background:var(--info)" }), "In progress"), h("span", { class: "right" }, "longest " + fmtDur(maxDur)))]
            : h("div", { class: "muted small", style: "padding:20px 0" }, "No runs yet."))));
  };
  const load = () => Promise.all([api("GET", "/api/targets/" + id + "/metrics"), api("GET", "/api/targets/" + id + "/health/history"), api("GET", "/api/targets/" + id + "/history")])
    .then((d) => { data = d; if (!c.querySelector(".chart:hover")) paint(); }, () => {});
  load();
  const timer = setInterval(load, 5000);
  return { el: c, cleanup: () => clearInterval(timer) };
}

// ---- security ----
function secBody(id) {
  const c = h("div");
  let loadedKey = null, scan = null, scans = [];
  const paint = () => {
    const t = T(id); if (!t) return;
    const out = [h("div", { class: "rowf", style: "margin-bottom:16px" },
      h("div", { class: "grow muted", style: "flex:1" }, "Runs ", h("span", { class: "mono" }, "bandit"), " (static analysis) and ", h("span", { class: "mono" }, "pip-audit"), " (known-vulnerable dependencies) against the working directory."),
      btn(t.scan_running ? "Scanning\u2026" : "Run scan", () => doScan(t), "hl", "shield", { disabled: t.scan_running }))];
    if (!scan) out.push(empty("No scan yet", "Run a scan to see findings here. Both tools must be installed: pip install bandit pip-audit"));
    else {
      const p = scan.raw_payload || {}, sev = p.bandit_severity || {};
      const deps = (p.pip_audit && (p.pip_audit.dependencies || p.pip_audit)) || [];
      const vuln = Array.isArray(deps) ? deps.filter((d) => d.vulns && d.vulns.length) : [];
      out.push(h("div", { class: "grid g-tiles" },
        tile("High severity", sev.high || 0, "counted as critical", sev.high ? "err" : null), tile("Medium severity", sev.medium || 0, null, sev.medium ? "warn" : null), tile("Low severity", sev.low || 0),
        tile("Vulnerable packages", vuln.length, Array.isArray(deps) ? deps.length + " audited" : "", vuln.length ? "err" : null), tile("Scanned", when(parseUTC(scan.scanned_at)), scans.length + " report(s) stored")));
      if (scans.length > 1) out.push(h("h3", {}, "History"), table(["Scanned", "Critical issues", "Vulnerable packages"], scans.slice(0, 8).map((x) => [fmtDT(parseUTC(x.scanned_at)), x.critical_issues_found, x.vulnerable_packages_found])));
      out.push(h("h3", {}, "Static analysis (bandit)"));
      if (p.bandit_error) out.push(h("div", { class: "note-box err" }, p.bandit_error));
      else {
        const order = { HIGH: 0, MEDIUM: 1, LOW: 2 };
        const res = ((p.bandit && p.bandit.results) || []).slice().sort((x, y) => (order[x.issue_severity] ?? 3) - (order[y.issue_severity] ?? 3)).slice(0, 300);
        out.push(res.length ? table(["Severity", "Confidence", "Test", "Location", "Issue"], res.map((r) => [
          h("span", { class: "tag " + ({ HIGH: "err", MEDIUM: "warn" }[r.issue_severity] || "") }, r.issue_severity), h("span", { class: "dim small" }, r.issue_confidence || ""), h("span", { class: "mono" }, r.test_id || ""),
          h("span", { class: "mono", style: "word-break:break-all" }, String(r.filename || "").replace(t.working_directory, ".") + ":" + r.line_number), r.issue_text || ""])) : h("div", { class: "note-box ok" }, "No issues found."));
      }
      out.push(h("h3", {}, "Dependencies (pip-audit)", p.pip_audit_mode ? h("span", { class: "dim small", style: "font-weight:400" }, "  auditing " + p.pip_audit_mode) : null));
      if (p.pip_audit_error) out.push(h("div", { class: "note-box err" }, p.pip_audit_error));
      else out.push(vuln.length ? table(["Package", "Installed", "Advisories", "Fixed in"], vuln.map((d) => [h("b", {}, d.name), h("span", { class: "mono" }, d.version), d.vulns.map((v) => v.id).join(", "),
        h("span", { class: "mono" }, [...new Set(d.vulns.flatMap((v) => v.fix_versions || []))].join(", ") || "-")])) : h("div", { class: "note-box ok" }, "No known vulnerabilities in " + (Array.isArray(deps) ? deps.length : 0) + " audited package(s)."));
    }
    c.replaceChildren(...out);
  };
  const update = () => {
    const t = T(id); if (!t) return;
    const key = (t.scan ? t.scan.id : 0) + "|" + t.scan_running;
    if (key === loadedKey) return paint();
    loadedKey = key;
    Promise.all([api("GET", "/api/targets/" + id + "/scan"), api("GET", "/api/targets/" + id + "/scans")]).then(([a, b]) => { scan = a; scans = b; paint(); }, () => paint());
  };
  update();
  return { el: c, update };
}

