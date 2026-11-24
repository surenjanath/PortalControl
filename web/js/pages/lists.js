"use strict";
// ------------------------------------------------------------- global pages --
function deploymentsView() {
  const def = { key: "deployments" };
  def.build = () => {
    const body = h("div");
    let runs = null;
    const fStatus = h("select", { style: "width:auto", "aria-label": "Status", onchange: () => { ui.depStatus = fStatus.value; paint(); } },
      h("option", { value: "all" }, "All statuses"), h("option", { value: "SUCCESS" }, "Success"), h("option", { value: "FAILED" }, "Failed"), h("option", { value: "RUNNING" }, "In progress"));
    const fTarget = h("select", { style: "width:auto", "aria-label": "Target", onchange: () => { ui.depTarget = fTarget.value; paint(); } });
    const paint = () => {
      if (fTarget.options.length !== targets.length + 1) {
        fTarget.replaceChildren(h("option", { value: "all" }, "All targets"), ...targets.map((t) => h("option", { value: t.id }, t.name)));
        fTarget.value = ui.depTarget; if (fTarget.value !== String(ui.depTarget)) { ui.depTarget = "all"; fTarget.value = "all"; }
      }
      fStatus.value = ui.depStatus;
      if (!runs) return body.replaceChildren(h("p", { class: "muted" }, "Loading\u2026"));
      const list = runs.filter((r) => (ui.depStatus === "all" || r.status === ui.depStatus) && (ui.depTarget === "all" || String(r.target_id) === String(ui.depTarget)));
      body.replaceChildren(...(list.length ? list.map((r) => depRow(r, true)) : [empty(runs.length ? "No matching deployments" : "No deployments yet", runs.length ? "Change the filters to see more." : "Runs of every target appear here.")]));
    };
    def.update = () => api("GET", "/api/runs?limit=200").then((x) => { runs = x; paint(); }, () => {});
    paint(); def.update();
    return h("div", {}, h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Deployments"), h("p", { class: "sub" }, "The most recent 200 runs across every target."))),
      h("div", { class: "rowf", style: "margin-bottom:14px" }, fStatus, fTarget), body);
  };
  return def;
}
// The security posture (posture.rs): score, grade and what to improve, most
// serious first; the same findings make up the security assessment report.
const POSTURE_LEVEL = { high: ["err", "High"], medium: ["warn", "Medium"], low: ["info", "Low"] };
function postureBox(p, err) {
  if (err) return h("div", { class: "note-box err" }, "The security checks could not be worked out: " + err);
  if (!p) return h("div", { class: "box muted", style: "margin-bottom:18px" }, "Working out the security checks…");
  const ps = p.posture;
  const cls = ps.grade === "A" || ps.grade === "B" ? "ok" : ps.grade === "C" ? "warn" : "err";
  const shown = ps.findings.slice(0, 12);
  return h("div", { class: "box posture", style: "margin-bottom:18px" },
    h("div", { class: "posture-top" },
      h("div", { class: "grade " + cls, title: "100 minus 15 per high, 6 per medium and 2 per low finding" }, ps.grade, h("small", {}, ps.score + " / 100")),
      h("div", { class: "grow" }, h("b", {}, ps.findings.length ? ps.findings.length + " thing" + (ps.findings.length === 1 ? "" : "s") + " to improve" : "Nothing to improve was found"),
        h("div", { class: "muted small", style: "margin-top:4px" }, ps.high + " high · " + ps.medium + " medium · " + ps.low + " low · " + p.attacks_30d + " attack" + (p.attacks_30d === 1 ? "" : "s") + " in the last 30 days"),
        h("div", { class: "rowf", style: "margin-top:10px" }, reportLink("/api/reports/security.pdf", "Security assessment (PDF)", "sm"))),
      h("div", { class: "posture-checks" }, ps.checks.map((c) => h("div", { title: c.detail }, h("span", { class: "dot " + (c.ok ? "ok" : "warn") }), c.name, h("span", { class: "dim" }, " · " + c.detail))))),
    shown.map((f) => {
      const [lc, lw] = POSTURE_LEVEL[f.level] || ["", f.level];
      return h("div", { class: "attn-row", style: "align-items:flex-start" }, h("span", { class: "tag " + lc, style: "min-width:58px;text-align:center" }, lw),
        h("div", { class: "grow" }, h("div", {}, f.target_id ? h("a", { class: "nm", href: "#/target/" + f.target_id + "/security" }, f.area) : h("b", {}, f.area), " — ", h("b", {}, f.title)),
          h("div", { class: "muted small", style: "margin-top:2px" }, f.detail), h("div", { class: "small", style: "margin-top:2px" }, h("b", {}, "Fix: "), f.fix)));
    }),
    ps.findings.length > shown.length ? h("div", { class: "dim small", style: "padding-top:8px" }, "+ " + (ps.findings.length - shown.length) + " more in the security assessment report") : null);
}
function securityView() {
  let posture = null, perr = null, at = 0;
  const top = h("div");
  const load = () => {
    at = Date.now();
    api("GET", "/api/security/posture").then((x) => { posture = x; perr = null; top.replaceChildren(postureBox(posture, perr)); },
      (e) => { perr = e.message; top.replaceChildren(postureBox(posture, perr)); });
  };
  return simple("security", () => {
    // The checks read files and the database: once a minute, not on every refresh.
    if (Date.now() - at > 60000) load();
    if (!top.childNodes.length) top.replaceChildren(postureBox(posture, perr));
    const scanned = targets.filter((t) => t.scan);
    return [
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Security"), h("p", { class: "sub" }, "What to improve, worked out from PortalControl's settings and every target's scans, configuration and attacks; then the scan results per target.")),
        btn("Scan all targets", () => targets.filter((t) => !t.scan_running).forEach(doScan), "hl", "shield", { disabled: !targets.length })),
      top,
      h("div", { class: "grid g-tiles", style: "margin-bottom:18px" },
        tile("Targets scanned", [scanned.length, h("small", {}, " / " + targets.length)]),
        tile("Critical issues", scanned.reduce((n, t) => n + t.scan.critical_issues_found, 0), "high-severity bandit findings", scanned.some((t) => t.scan.critical_issues_found) ? "err" : null),
        tile("Vulnerable packages", scanned.reduce((n, t) => n + t.scan.vulnerable_packages_found, 0), "reported by pip-audit", scanned.some((t) => t.scan.vulnerable_packages_found) ? "err" : null)),
      targets.length ? table(["Target", "Result", "Critical issues", "Vulnerable packages", "Scanned", ""], targets.map((t) => [
        h("a", { href: "#/target/" + t.id + "/security", style: "font-weight:600" }, t.name), secTag(t) || h("span", { class: "dim" }, "not scanned"),
        t.scan ? t.scan.critical_issues_found : "-", t.scan ? t.scan.vulnerable_packages_found : "-", t.scan ? when(parseUTC(t.scan.scanned_at)) : "-",
        h("div", { class: "rowf", style: "justify-content:flex-end" }, btn(t.scan_running ? "Scanning\u2026" : "Scan", () => doScan(t), "sm", null, { disabled: t.scan_running }), h("a", { class: "btn sm", href: "#/target/" + t.id + "/security" }, "Report"))]))
        : empty("No targets yet", "Register a target to scan it."),
    ];
  });
}
function activityView() {
  const def = { key: "activity" };
  def.build = () => {
    const body = h("div", { class: "box" });
    const sel = h("select", { style: "width:auto", "aria-label": "Level", onchange: () => { ui.actLevel = sel.value; def.update(); } },
      h("option", { value: "all" }, "All events"), h("option", { value: "problems" }, "Warnings and errors"), h("option", { value: "error" }, "Errors only"));
    sel.value = ui.actLevel;
    def.update = () => {
      const list = activity.filter((e) => ui.actLevel === "all" || e.level === "error" || (ui.actLevel === "problems" && e.level === "warn"));
      body.replaceChildren(...(list.length ? list.map((e) => feedRow(e, true)) : [h("span", { class: "muted" }, activity.length ? "No events at this level." : "Nothing has happened since the manager started.")]));
    };
    def.update();
    return h("div", {}, h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Activity"), h("p", { class: "sub" }, "Deployments, health changes, Git updates, scans and automation, newest first. Kept in memory (last 300 events) since the manager started."))),
      h("div", { style: "margin-bottom:14px" }, sel), body);
  };
  return def;
}
