"use strict";
// --------------------------------------------------------------- incidents --
// What the monitor found (monitor.rs): the rule that fired, the local AI
// model's explanation, and its suggested fix as a button. Nothing is changed
// until someone clicks it.
const SEV = { critical: ["err", "Critical"], warning: ["warn", "Warning"], info: ["info", "Info"] };
function incidentCard(i, reload) {
  const [cls, word] = SEV[i.severity] || ["", i.severity];
  const t = i.target_id ? T(i.target_id) : null;
  const status = (s, msg) => act(async () => { await api("POST", "/api/incidents/" + i.id + "/status", { status: s }); reload(); }, msg);
  const fix = async () => {
    if (!(await confirmDlg("Apply the suggested fix?", [h("p", {}, h("b", {}, i.action_label)), i.action_reason ? h("p", { class: "muted" }, i.action_reason) : null,
      h("p", { class: "muted small" }, "It runs like the same button elsewhere in PortalControl, and its output goes to the target's log.")], "Apply fix"))) return;
    act(async () => { const r = await api("POST", "/api/incidents/" + i.id + "/act"); toast(r.note); reload(); });
  };
  const open = i.status === "open";
  return h("div", { class: "box incident" + (open ? "" : " closed") },
    h("div", { class: "rowf", style: "gap:10px;align-items:flex-start" },
      h("span", { class: "pill " + cls }, h("span", { class: "dot " + cls }), word),
      h("div", { style: "flex:1;min-width:0" },
        h("div", { class: "inc-title" }, i.title),
        h("div", { class: "dim small" }, i.trace_id ? [h("span", { class: "trace-chip", title: "Open the trace: log lines → rule → AI → email → fix", onclick: () => openTrace(i.trace_id) }, i.trace_id), " "] : null,
          t ? h("a", { href: "#/target/" + t.id }, t.name) : "removed target", " · #" + i.id + " · ", when(parseUTC(i.created_at)), " · " + i.trigger.replace("_", " "),
          i.run_id && t ? [" · ", h("a", { href: "#/target/" + t.id + "/deployments/" + i.run_id }, "run #" + i.run_id)] : null,
          i.status !== "open" ? " · " + i.status : "")),
      open ? h("div", { class: "rowf", style: "flex-wrap:nowrap" }, btn("Resolve", () => status("resolved", "Marked resolved"), "sm"), btn("Dismiss", () => status("dismissed", "Dismissed"), "sm ghost"))
        : btn("Reopen", () => status("open", "Reopened"), "sm ghost")),
    i.summary ? h("p", { class: "inc-text" }, i.summary) : null,
    i.cause ? h("p", { class: "inc-text" }, h("b", {}, "Likely cause: "), i.cause) : null,
    i.action !== "none" && i.action_label ? h("div", { class: "inc-fix" },
      h("div", { style: "flex:1" }, h("b", {}, "Suggested fix: "), i.action_label, i.action_reason ? h("div", { class: "muted small" }, i.action_reason) : null),
      open ? btn("Apply fix", fix, "hl sm", "play") : null)
      : i.action_reason ? h("p", { class: "inc-text muted" }, h("b", {}, "What to do: "), i.action_reason) : null,
    i.note ? h("div", { class: "dim small", style: "margin-top:6px" }, i.note) : null,
    h("div", { class: "dim small", style: "margin-top:8px" }, i.analysed_by ? "Analysed by " + i.analysed_by + " on this machine" : "Monitoring rules only (no AI model set up)"),
    i.excerpt ? h("details", { style: "margin-top:8px" }, h("summary", { class: "dim small", style: "cursor:pointer" }, "Log lines it looked at"),
      h("pre", { class: "log", style: "max-height:260px;margin-top:6px" }, i.excerpt)) : null);
}
function incidentsView() {
  const def = { key: "incidents" };
  let list = [];
  const body = h("div");
  const load = () => api("GET", "/api/incidents?limit=200" + (ui.incOpen ? "&status=open" : "")).then((x) => { list = x; paint(); }, (e) => toast(e.message, true));
  const filter = h("div", { class: "rowf" });
  const paint = () => {
    filter.replaceChildren(btn("Open", () => { ui.incOpen = true; load(); }, "sm" + (ui.incOpen ? " on" : "")), btn("All", () => { ui.incOpen = false; load(); }, "sm" + (ui.incOpen ? "" : " on")));
    body.replaceChildren(list.length ? list.map((i) => incidentCard(i, load))
      : empty(ui.incOpen ? "No open incidents" : "No incidents yet", "The monitor watches every target's output and opens an incident when something looks wrong: a failure, a crash, a burst of errors or a new kind of error.",
          h("a", { class: "btn", href: "#/settings/monitor" }, icon("settings"), "Monitor settings")));
  };
  let flow = null;
  def.build = () => { load(); flow = monitorFlow(); return h("div", {},
    h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Incidents"), h("p", { class: "sub" }, "Problems the monitor found, explained by the AI model on this machine, with a suggested fix you can apply.")),
      filter, h("a", { class: "btn", href: "#/settings/monitor" }, icon("settings"), "Settings")), flow.el, body); };
  const sink = (m) => { if (m.type === "incident") load(); };
  sinks.add(sink);
  def.cleanup = () => { sinks.delete(sink); if (flow) flow.cleanup(); };
  // Repaint on every data refresh, so target names appear once they are loaded.
  def.update = () => paint();
  return def;
}
