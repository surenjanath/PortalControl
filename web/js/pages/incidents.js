"use strict";
// --------------------------------------------------------------- incidents --
// What the monitor found (monitor.rs): the rule that fired, the explanation
// (the local AI model's, or the rules' own diagnosis when there is no model),
// the key error line, steps to fix it, the state of the target at that moment,
// and a suggested fix as a button. Nothing is changed until someone clicks it.
// Incidents can be selected and resolved, dismissed, reopened or deleted in bulk.
const SEV = { critical: ["err", "Critical"], warning: ["warn", "Warning"], info: ["info", "Info"] };
const INC_KINDS = { failure: "Failed run", crash_loop: "Crash loop", health_down: "Health check down", deploy_failed: "Deployment failed",
  error_burst: "Burst of errors", new_error: "New error", attack: "Attack", brute_force: "Password guessing", flood: "Flood of requests" };
const INC_SECURITY = ["attack", "brute_force", "flood"];
const INC_SOURCE = { ai: "AI analysis", rules: "Rules diagnosis", rules_ai_failed: "Rules diagnosis — the AI model did not answer" };

// The target's state when the incident was opened, from its details (0.14.0).
function incidentFacts(i) {
  const d = i.details || {};
  const c = d.context || {}, r = d.run, past = d.history || {}, sig = d.signals || {};
  const rows = [];
  if (r) {
    const result = r.status === "SUCCESS" ? "succeeded" : r.status === "RUNNING" ? "still running" : "failed" + (r.exit_code !== null && r.exit_code !== undefined ? " (exit " + r.exit_code + ")" : "");
    rows.push(["Run", [i.target_id ? h("a", { href: "#/target/" + i.target_id + "/deployments/" + r.id }, "#" + r.id) : "#" + r.id,
      " · " + trig(r.triggered_by) + " · " + result + (r.duration_ms ? " · " + fmtDur(r.duration_ms) : "")]]);
    if (r.commit_sha) rows.push(["Deployed commit", [h("span", { class: "mono" }, short(r.commit_sha)), " ", (r.commit_message || "").split("\n")[0]]]);
  }
  rows.push(["Process then", (c.running ? "running" : "not running") + (c.mode ? " · " + (c.mode === "SERVICE" ? "service" : "task") : "")]);
  if (c.health) rows.push(["Health check then", c.health.status + (c.health.latency_ms !== null && c.health.latency_ms !== undefined ? " · " + c.health.latency_ms + " ms" : "") + (c.health.detail ? " · " + c.health.detail : "")]);
  if (c.git) rows.push(["Git then", (c.git.branch || "detached") + " · " + short(c.git.head) + (c.git.behind ? " · " + c.git.behind + " commit(s) waiting" : "") + (c.git.dirty_files ? " · " + c.git.dirty_files + " local change(s)" : "")]);
  if (sig.lines) rows.push(["Log lines examined", sig.lines + (sig.error_lines ? " · " + sig.error_lines + " look like errors" : "")]);
  rows.push(["Happened before", past.same_7d ? past.same_24h + " time(s) in 24 h, " + past.same_7d + " in 7 days" + (past.previous_at ? " · last " + fmtAgo(parseUTC(past.previous_at)) : "") : "not in the 7 days before"]);
  if (sig.rule) rows.push(["Rule that fired", sig.rule]);
  return h("div", { class: "inc-grid" }, rows.map(([k, v]) => h("div", {}, h("div", { class: "k" }, k), h("div", { class: "v" }, v))));
}

function incidentCard(i, ctx) {
  const [cls, word] = SEV[i.severity] || ["", i.severity];
  const t = i.target_id ? T(i.target_id) : null;
  const d = i.details || {};
  const ai = d.ai || null, diag = d.diagnosis || null, sig = d.signals || {};
  const status = (s, msg) => act(async () => { await api("POST", "/api/incidents/" + i.id + "/status", { status: s }); ctx.reload(); }, msg);
  const fix = async () => {
    if (!(await confirmDlg("Apply the suggested fix?", [h("p", {}, h("b", {}, i.action_label)), i.action_reason ? h("p", { class: "muted" }, i.action_reason) : null,
      h("p", { class: "muted small" }, "It runs like the same button elsewhere in PortalControl, and its output goes to the target's log.")], "Apply fix"))) return;
    act(async () => { const r = await api("POST", "/api/incidents/" + i.id + "/act"); toast(r.note); ctx.reload(); });
  };
  const open = i.status === "open";
  const picked = ctx.picked.has(i.id);
  const box = h("input", { type: "checkbox", class: "inc-check", checked: picked, "aria-label": "Select incident " + i.id, onchange: () => ctx.pick(i.id, box.checked) });
  const keyError = (ai && ai.key_error) || sig.key_error;
  const steps = ai && ai.steps && ai.steps.length ? ai.steps : diag && diag.steps ? diag.steps : [];
  const fromAi = d.source === "ai";
  // Without details (incidents from before 0.14.0) the stored text is all there is.
  const legacy = !i.details;
  return h("div", { class: "box incident" + (open ? "" : " closed") + (picked ? " picked" : "") },
    h("div", { class: "inc-head" }, box,
      h("span", { class: "pill " + cls }, h("span", { class: "dot " + cls }), word),
      h("div", { style: "flex:1;min-width:0" },
        h("div", { class: "inc-title" }, i.title),
        h("div", { class: "dim small" }, i.trace_id ? [h("span", { class: "trace-chip", title: "Open the trace: log lines → rule → AI → email → fix", onclick: () => openTrace(i.trace_id) }, i.trace_id), " "] : null,
          t ? h("a", { href: "#/target/" + t.id }, t.name) : "removed target", " · #" + i.id + " · ", when(parseUTC(i.created_at)), " · " + (INC_KINDS[i.trigger] || i.trigger),
          i.status !== "open" ? " · " + i.status : "")),
      open ? h("div", { class: "rowf", style: "flex-wrap:nowrap" }, btn("Resolve", () => status("resolved", "Marked resolved"), "sm"), btn("Dismiss", () => status("dismissed", "Dismissed"), "sm ghost"))
        : btn("Reopen", () => status("open", "Reopened"), "sm ghost")),
    i.summary ? h("p", { class: "inc-text" }, i.summary) : null,
    keyError ? h("div", { class: "inc-key", title: "The log line that shows the problem" }, keyError) : null,
    i.cause ? h("p", { class: "inc-text" }, h("b", {}, "Likely cause: "), i.cause) : null,
    steps.length ? h("div", { class: "inc-text" }, h("b", {}, "What to do"), h("ol", { class: "inc-steps" }, steps.map((s) => h("li", {}, s))))
      : legacy && i.action_reason && i.action === "none" ? h("p", { class: "inc-text muted" }, h("b", {}, "What to do: "), i.action_reason) : null,
    i.action !== "none" && i.action_label ? h("div", { class: "inc-fix" },
      h("div", { style: "flex:1" }, h("b", {}, "Suggested fix: "), i.action_label, i.action_reason && (legacy || fromAi) ? h("div", { class: "muted small" }, i.action_reason) : null),
      open ? btn("Apply fix", fix, "hl sm", "play") : null) : null,
    legacy ? null : incidentFacts(i),
    i.note ? h("div", { class: "dim small", style: "margin-top:8px" }, i.note) : null,
    h("div", { class: "dim small inc-src", style: "margin-top:10px" },
      legacy ? (i.analysed_by ? "Analysed by " + i.analysed_by + " on this machine" : "Monitoring rules only (no AI model set up)")
        : [h("span", { class: "tag " + (fromAi ? "acc" : "") }, INC_SOURCE[d.source] || "Rules diagnosis"),
          fromAi ? " " + i.analysed_by + " on this machine" : diag ? " " + diag.title : "",
          ai && ai.confidence ? h("span", { class: "tag " + (ai.confidence === "high" ? "ok" : ai.confidence === "low" ? "warn" : "") }, "confidence " + ai.confidence) : null]),
    fromAi && diag && diag.matched ? h("details", { style: "margin-top:8px" }, h("summary", { class: "dim small", style: "cursor:pointer" }, "What the rules recognised"),
      h("div", { class: "inc-text" }, h("b", {}, diag.title + ". "), diag.cause), h("ol", { class: "inc-steps" }, diag.steps.map((s) => h("li", {}, s)))) : null,
    i.excerpt ? h("details", { style: "margin-top:8px" }, h("summary", { class: "dim small", style: "cursor:pointer" }, INC_SECURITY.includes(i.trigger) ? "Evidence it looked at" : "Log lines it looked at"),
      h("pre", { class: "log", style: "max-height:300px;margin-top:6px" }, i.excerpt)) : null);
}

function incidentsView() {
  const def = { key: "incidents" };
  const f = ui.incFilter || (ui.incFilter = { status: "open", severity: "", kind: "", target: "", q: "" });
  let list = [];
  const picked = new Set();
  const body = h("div"), bar = h("div", { class: "bulkbar" }), filterBox = h("div");
  const counts = h("div", { class: "grid g-tiles", style: "margin-bottom:14px" });
  const load = () => api("GET", "/api/incidents?limit=500").then((x) => {
    list = x;
    const ids = new Set(list.map((i) => i.id));
    [...picked].forEach((id) => { if (!ids.has(id)) picked.delete(id); });
    paint();
  }, (e) => toast(e.message, true));
  const shown = () => list.filter((i) => (!f.status || i.status === f.status) && (!f.severity || i.severity === f.severity)
    && (!f.kind || (f.kind === "security" ? INC_SECURITY.includes(i.trigger) : f.kind === "fault" ? !INC_SECURITY.includes(i.trigger) : i.trigger === f.kind))
    && (!f.target || String(i.target_id) === f.target)
    && (!f.q || (i.title + " " + i.summary + " " + i.cause + " " + (i.trace_id || "")).toLowerCase().includes(f.q.toLowerCase())));

  const select = (key, options, label) => {
    const el = h("select", { "aria-label": label, onchange: () => { f[key] = el.value; paint(); } }, options.map(([v, text]) => h("option", { value: v }, text)));
    el.value = f[key];
    return el;
  };
  const search = h("input", { type: "search", placeholder: "Search titles and causes…", value: f.q, style: "max-width:220px", "aria-label": "Search incidents",
    oninput: () => { f.q = search.value; paint(); } });
  // The search box is kept, so typing in it is not interrupted by a repaint.
  const filters = () => {
    const tids = [...new Set(list.map((i) => i.target_id).filter((x) => x))];
    return h("div", { class: "rowf inc-filters" },
      select("status", [["open", "Open"], ["", "All statuses"], ["resolved", "Resolved"], ["dismissed", "Dismissed"]], "Status"),
      select("severity", [["", "All severities"], ["critical", "Critical"], ["warning", "Warning"], ["info", "Info"]], "Severity"),
      select("kind", [["", "All kinds"], ["fault", "Faults"], ["security", "Security"]].concat(Object.entries(INC_KINDS)), "Kind"),
      select("target", [["", "All targets"]].concat(tids.map((id) => [String(id), (T(id) && T(id).name) || "#" + id])), "Target"),
      search);
  };

  const bulk = async (action) => {
    const ids = [...picked];
    if (!ids.length) return;
    if (action === "delete" && !(await confirmDlg("Delete " + ids.length + " incident" + (ids.length === 1 ? "" : "s") + "?",
      [h("p", {}, "They are removed with their traces. This cannot be undone."), h("p", { class: "muted small" }, "To keep a record, resolve or dismiss them instead.")], "Delete", true))) return;
    act(async () => { const r = await api("POST", "/api/incidents/bulk", { ids, action }); picked.clear(); toast(r.message); await load(); });
  };
  const paintBar = (visible) => {
    const n = picked.size;
    const all = visible.length > 0 && visible.every((i) => picked.has(i.id));
    const toggle = h("input", { type: "checkbox", class: "inc-check", checked: all, "aria-label": "Select all shown",
      onchange: () => { visible.forEach((i) => (toggle.checked ? picked.add(i.id) : picked.delete(i.id))); paint(); } });
    bar.className = "bulkbar" + (n ? " on" : "");
    bar.replaceChildren(h("label", { class: "rowf", style: "gap:8px;cursor:pointer" }, toggle, h("span", {}, n ? n + " selected" : "Select all " + visible.length + " shown")),
      n ? h("div", { class: "rowf right" },
        btn("Resolve", () => bulk("resolved"), "sm"), btn("Dismiss", () => bulk("dismissed"), "sm"), btn("Reopen", () => bulk("open"), "sm ghost"),
        btn("Delete", () => bulk("delete"), "sm danger", "trash"), btn("Clear", () => { picked.clear(); paint(); }, "sm ghost")) : null);
  };
  const paintCounts = () => {
    const open = list.filter((i) => i.status === "open");
    const day = Date.now() - 86400000;
    counts.replaceChildren(
      tile("Open", open.length, open.filter((i) => i.severity === "critical").length + " critical", open.length ? "err" : null),
      tile("Last 24 hours", list.filter((i) => parseUTC(i.created_at) > day).length, "opened"),
      tile("Security", list.filter((i) => INC_SECURITY.includes(i.trigger)).length, "attacks, password guessing, floods", list.some((i) => INC_SECURITY.includes(i.trigger) && i.status === "open") ? "warn" : null),
      tile("Explained by AI", list.filter((i) => i.details && i.details.source === "ai").length, "the others by the built-in rules"));
  };
  const ctx = { picked, reload: () => load(), pick: (id, on) => { if (on) picked.add(id); else picked.delete(id); paint(); } };
  const paint = () => {
    const visible = shown();
    paintCounts();
    if (document.activeElement !== search) filterBox.replaceChildren(filters());
    paintBar(visible);
    body.replaceChildren(visible.length ? visible.map((i) => incidentCard(i, ctx))
      : empty(list.length ? "No incidents match" : "No incidents yet", list.length ? "Change the filters above to see the others."
        : "The monitor watches every target's output and opens an incident when something looks wrong: a failure, a crash, a burst of errors, a new kind of error or an attack.",
          list.length ? null : h("a", { class: "btn", href: "#/settings/monitor" }, icon("settings"), "Monitor settings")));
  };
  let flow = null;
  def.build = () => { load(); flow = monitorFlow(); return h("div", {},
    h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Incidents"), h("p", { class: "sub" }, "Problems the monitor found, why they happened and what to do, with a fix you can apply. Select several to close or delete them together.")),
      h("a", { class: "btn", href: "#/settings/monitor" }, icon("settings"), "Settings")), counts, filterBox, bar, body, h("h2", {}, "How the monitor works"), flow.el); };
  const sink = (m) => { if (m.type === "incident") load(); };
  sinks.add(sink);
  def.cleanup = () => { sinks.delete(sink); if (flow) flow.cleanup(); };
  // Repaint on every data refresh, so target names appear once they are loaded.
  def.update = () => paint();
  return def;
}
