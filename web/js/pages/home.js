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
      t.health_url ? [strip(t, 24), h("span", {}, t.health ? (t.health.status === "DOWN" ? "down" : t.health.latency_ms + " ms") : "checking")] : h("span", { class: "dim" }, "no health probe"),
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
function dashboardView() {
  let runs = [];
  const def = { key: "dashboard" };
  let c;
  const paint = () => {
    if (!loaded) return c.replaceChildren(h("p", { class: "muted" }, "Loading\u2026"));
    const running = targets.filter((t) => t.running).length;
    const probed = targets.filter((t) => t.health_url);
    const healthy = probed.filter((t) => t.health && t.health.status !== "DOWN").length;
    const updates = targets.filter((t) => t.git && t.git.is_repo && t.git.behind > 0).length;
    const failed = targets.filter((t) => tStatus(t).cls === "err").length;
    const issues = targets.reduce((n, t) => n + (t.scan ? t.scan.critical_issues_found + t.scan.vulnerable_packages_found : 0), 0);
    const hl = sys && sys.host && sys.host.latest;
    const memPct = hl && sys.host.mem_total ? (hl.mem_used / sys.host.mem_total) * 100 : 0;
    c.replaceChildren(
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Dashboard"), h("p", { class: "sub" }, "Deployments, health and Git state of every registered project on this machine.")),
        h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target")),
      h("div", { class: "grid g-tiles" },
        tile("Targets", [running, h("small", {}, " / " + targets.length + " running")], targets.filter((t) => t.run_mode === "SERVICE").length + " service(s), " + targets.filter((t) => t.run_mode !== "SERVICE").length + " task(s)"),
        tile("Health", probed.length ? [healthy, h("small", {}, " / " + probed.length + " reachable")] : "-", probed.length ? "probed every 15 s" : "no health URLs configured", probed.length && healthy < probed.length ? "err" : null),
        tile("Updates available", updates, updates ? "new commits on origin" : "all repositories current", updates ? "info" : null),
        tile("Failing", failed, failed ? "last run failed or service exited" : "nothing is failing", failed ? "err" : null),
        tile("Security findings", issues, targets.some((t) => t.scan) ? "critical issues + vulnerable packages" : "no scans run yet", issues ? "warn" : null)),
      hl ? h("div", { class: "grid g-tiles", style: "margin-top:12px" },
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "Host CPU"), h("div", { class: "val" }, hl.cpu.toFixed(0), h("small", {}, " %")), h("div", { class: "bar" }, h("i", { style: "width:" + Math.min(100, hl.cpu) + "%" })), h("div", { class: "note" }, sys.host.cpus + " logical cores")),
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "Host memory"), h("div", { class: "val" }, fmtBytes(hl.mem_used), h("small", {}, " / " + fmtBytes(sys.host.mem_total))), h("div", { class: "bar" }, h("i", { style: "width:" + memPct + "%" })), h("div", { class: "note" }, memPct.toFixed(0) + "% in use")),
        h("div", { class: "box tile" }, h("div", { class: "lbl" }, "PortalControl"), h("div", { class: "val" }, fmtBytes(hl.manager_mem)), h("div", { class: "note" }, "manager memory \u00b7 up " + fmtUp(sys.uptime_secs)))) : null,
      targets.length ? byProject(targets).map(([name, list]) => [
        h("h2", {}, h("a", { class: "plink", href: projHref(name) }, name), h("span", { class: "dim small", style: "font-weight:400" }, "  " + list.length + " process" + (list.length === 1 ? "" : "es")),
          h("a", { class: "dim small", href: projHref(name), style: "font-weight:400;margin-left:10px" }, "terminals")),
        h("div", { class: "grid g-cards" }, list.map(targetCard))])
        : [h("h2", {}, "Projects"), empty("No targets yet", "Point PortalControl at a project folder and its deploy script to get started.", h("a", { class: "btn hl", href: "#/new" }, icon("plus"), "New target"))],
      h("div", { class: "grid g2", style: "margin-top:8px" },
        h("div", {}, h("h2", {}, "Recent deployments", h("a", { href: "#/deployments", class: "dim small", style: "font-weight:400;margin-left:10px" }, "view all")),
          runs.length ? runs.slice(0, 6).map((r) => depRow(r, true)) : h("div", { class: "box muted" }, "Nothing has been deployed yet.")),
        h("div", {}, h("h2", {}, "Activity", h("a", { href: "#/activity", class: "dim small", style: "font-weight:400;margin-left:10px" }, "view all")),
          h("div", { class: "box" }, activity.length ? activity.slice(0, 8).map((e) => feedRow(e, true)) : h("span", { class: "muted" }, "No activity since the manager started.")))));
  };
  def.build = () => { c = h("div"); paint(); def.update(); return c; };
  def.update = () => { paint(); api("GET", "/api/runs?limit=6").then((x) => { runs = x; if (view.key === def.key) paint(); }, () => {}); };
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

