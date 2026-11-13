"use strict";
// ----------------------------------------------------------- project page --
// One project = the processes that belong together (web server, queue
// cluster, workers ...), with group actions and their terminals side by side.
function projectView(name) {
  const def = { key: "project/" + name };
  def.build = () => {
    // In start order, the same order the server uses for "Start all".
    const members = () => targets.filter((t) => t.project === name).sort((a, b) => a.start_order - b.start_order || a.id - b.id);
    const list = members();
    if (!list.length) return loaded ? empty("Project not found", "No target belongs to a project called “" + name + "”.", h("a", { class: "btn", href: "#/projects" }, "Back to projects")) : h("p", { class: "muted" }, "Loading…");
    const idsKey = list.map((t) => t.id).join(",");
    const color = new Map(list.map((t, i) => [t.id, i]));
    // "Claims Portal · Django-Q cluster" is labelled by its role in the merged view.
    const role = (t) => (t.name.includes(" · ") ? t.name.split(" · ").pop() : t.name);
    const tagOf = (t) => { const r = role(t); return { label: r.length > 20 ? r.slice(0, 19) + "…" : r, color: color.get(t.id) }; };
    const head = h("div"), rows = h("div");
    const each = async (pick, call, msg) => {
      const todo = members().filter(pick);
      if (!todo.length) return toast("Nothing to do");
      const results = await Promise.allSettled(todo.map((t) => api("POST", "/api/targets/" + t.id + "/" + call)));
      const bad = results.filter((r) => r.status === "rejected");
      if (bad.length) toast(bad.length + " of " + todo.length + " failed: " + bad[0].reason.message, true); else toast(msg + " (" + todo.length + ")");
      scheduleRefresh(60);
    };
    const paint = () => {
      const now = members();
      const up = now.filter((t) => t.running).length;
      head.replaceChildren(
        h("div", { class: "crumbs" }, h("a", { href: "#/projects" }, "Projects"), icon("chevron"), name),
        h("div", { class: "head" },
          h("div", { class: "grow" }, h("div", { class: "rowf" }, h("h1", {}, name), h("span", { class: "pill " + (up === now.length ? "ok" : up ? "warn" : "off") }, up + " of " + now.length + " running")),
            h("p", { class: "sub" }, "Every process of this project, started and stopped together, with their terminals in one place.")),
          h("div", { class: "rowf" },
            // Start and stop go through the server so the start order is honoured.
            btn("Start all", () => act(async () => { const r = await api("POST", "/api/projects/" + encodeURIComponent(name) + "/start"); toast("Starting in order: " + r.starting.join(" → ")); }), "hl", "play"),
            btn("Restart all", () => each((t) => t.running, "restart", "Restarting"), "warnc", "restart"),
            btn("Stop all", () => act(() => api("POST", "/api/projects/" + encodeURIComponent(name) + "/stop"), "Stopping…"), "danger", "stop"),
            reportLink("/api/reports/project/" + encodeURIComponent(name), "PDF report"))));
      rows.replaceChildren(table(["Process", "Status", "Runs", "Resources", "Git", ""], now.map((t) => [
        h("div", {}, h("span", { class: "c" + (color.get(t.id) % 6), style: "font-weight:700" }, "● "), h("a", { href: "#/target/" + t.id, style: "font-weight:600" }, t.name), " ", h("span", { class: "tag" }, t.run_mode === "SERVICE" ? "Service" : "Task"),
          h("div", { class: "dim mono", style: "font-size:11.5px;margin-top:2px" }, t.script_name),
          h("a", { class: "dim small", href: "#/target/" + t.id + "/configuration/advanced", title: "Change the start order" }, "start order " + t.start_order)),
        pill(tStatus(t)),
        t.running ? "pid " + t.running.pid + " · up " + fmtUp(t.running.uptime_secs) : t.last_run ? ["last ", when(parseUTC(t.last_run.executed_at))] : "never",
        t.metrics ? t.metrics.cpu.toFixed(1) + "% CPU · " + fmtBytes(t.metrics.mem) : h("span", { class: "dim" }, "-"),
        gitTag(t) || h("span", { class: "dim" }, "-"),
        h("div", { class: "rowf", style: "justify-content:flex-end;flex-wrap:nowrap" }, actionButtons(t, true))])));
    };
    paint();

    // Terminals: one merged view plus one per process.
    const all = LogView(name.replace(/[^\w.-]+/g, "_") + "-all");
    const views = new Map(list.map((t) => [t.id, LogView(t.name.replace(/[^\w.-]+/g, "_"))]));
    const panes = h("div", {}, all.el, [...views.values()].map((v) => { v.el.style.display = "none"; return v.el; }));
    const tabs = h("div", { class: "termtabs" });
    let current = "all";
    const paintTabs = () => tabs.replaceChildren(
      btn("All processes", () => pick("all"), "sm" + (current === "all" ? " on" : "")),
      list.map((t) => h("button", { class: "btn sm" + (current === t.id ? " on" : ""), type: "button", onclick: () => pick(t.id) },
        h("span", { class: "c" + (color.get(t.id) % 6) }, "●"), t.name)));
    const pick = (which) => { current = which; all.el.style.display = which === "all" ? "" : "none"; views.forEach((v, id) => { v.el.style.display = which === id ? "" : "none"; }); paintTabs(); };
    paintTabs();
    list.forEach((t) => api("GET", "/api/targets/" + t.id + "/logs").then((d) => {
      views.get(t.id).setText(d.logs);
      // Older output has no timestamps, so it is shown per process, most recent lines only.
      (d.logs || "").replace(/\n$/, "").split("\n").filter(Boolean).slice(-150).forEach((line) => all.add(line, null, null, tagOf(t)));
    }, () => {}));
    const sink = (m) => {
      const t = m.target_id && views.has(m.target_id) ? T(m.target_id) : null;
      if (!t) return;
      if (m.type === "log") { views.get(t.id).add(m.line, m.stream, m.ts); all.add(m.line, m.stream, m.ts, tagOf(t)); }
      else if (m.type === "run_started") { const line = "──── run #" + m.run_id + " started ────"; views.get(t.id).add(line, "sep", Date.now()); all.add(line, "sep", Date.now(), tagOf(t)); }
    };
    sinks.add(sink);
    def.update = () => { if (members().map((t) => t.id).join(",") !== idsKey) return show(true); paint(); };
    def.cleanup = () => sinks.delete(sink);
    return h("div", {}, head, rows, h("h2", {}, "Terminals"), tabs, panes,
      h("p", { class: "hint", style: "margin-top:12px" }, "To add another process to this project, open one of its targets and use Configuration → Related Processes, or create a new target with the same project name."));
  };
  return def;
}

