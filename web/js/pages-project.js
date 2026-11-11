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
            btn("Stop all", () => act(() => api("POST", "/api/projects/" + encodeURIComponent(name) + "/stop"), "Stopping…"), "danger", "stop"))));
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

// -------------------------------------------------------------- new target --
function newView(cloneId) {
  const def = { key: "new/" + (cloneId || "") };
  // The form must survive background data refreshes, so there is nothing to repaint.
  def.update = () => {};
  def.build = () => {
    const src = cloneId ? T(cloneId) : null;
    const win = !sys || sys.os === "windows";
    const f = {
      dir: h("input", { type: "text", value: src ? src.working_directory : "", placeholder: win ? "C:\\Users\\ICT\\Projects\\claims_portal" : "/Users/ict/projects/claims_portal", autocomplete: "off", spellcheck: "false" }),
      name: h("input", { type: "text", maxlength: "100", value: src ? src.name + " (copy)" : "", placeholder: "Claims Portal" }),
      project: h("input", { type: "text", maxlength: "60", value: src ? src.project : "Default", list: "projlist" }),
      script: h("input", { type: "text", value: src ? src.script_name : "", placeholder: win ? "deploy.bat" : "deploy.sh", spellcheck: "false" }),
      branch: h("input", { type: "text", value: src ? src.git_branch : "main", spellcheck: "false" }),
      mode: h("select", {}, h("option", { value: "TASK" }, "Task \u2014 runs to completion (a deployment script)"), h("option", { value: "SERVICE" }, "Service \u2014 long-running process (a server)")),
      health: h("input", { type: "text", value: src && src.health_url ? src.health_url : "", placeholder: "http://127.0.0.1:8000/", spellcheck: "false" }),
      desc: h("input", { type: "text", maxlength: "500", value: src ? src.description : "", placeholder: "Optional" }),
    };
    if (src) f.mode.value = src.run_mode;
    let nameTouched = !!src, branchTouched = !!src, timer = null, seq = 0;
    let workers = [];            // detected companions, each with its checkbox
    const workerBox = h("div");
    f.name.oninput = () => (nameTouched = true);
    f.branch.oninput = () => (branchTouched = true);
    const found = h("div");
    const err = h("span", { class: "errtxt" });
    async function inspect() {
      const dir = f.dir.value.trim();
      if (!dir) return found.replaceChildren();
      const mine = ++seq;
      let info;
      try { info = await api("GET", "/api/fs/inspect?dir=" + encodeURIComponent(dir)); } catch (e) { return; }
      if (mine !== seq) return;
      const registered = targets.filter((o) => o.working_directory === dir).map((o) => o.script_name.toLowerCase());
      workers = (info.workers || []).filter((w) => !registered.includes(w.script.toLowerCase())).map((w) => ({ w, box: h("input", { type: "checkbox", checked: !src }) }));
      workerBox.replaceChildren(workers.length ? [h("div", { class: "hint", style: "margin:-4px 0 8px" }, "This project also needs background processes. Tick the ones to register alongside it (same project, restart on crash):"),
        workers.map(({ w, box }) => check(box, "Also add: " + w.label, w.command + (w.script_exists ? "" : "  — creates " + w.script + " in the folder")))] : null);
      if (!info.exists) return found.replaceChildren(h("div", { class: "note-box err" }, info.error || "Directory not found on this machine."));
      if (!nameTouched && info.suggested_name) f.name.value = info.suggested_name;
      if (!branchTouched && info.branch) f.branch.value = info.branch;
      if (!f.script.value && info.scripts.length === 1) f.script.value = info.scripts[0];
      found.replaceChildren(h("div", { class: "note-box ok" },
        h("div", { class: "rowf" }, h("b", { style: "color:var(--text)" }, "Directory found"), info.detected.map((d) => h("span", { class: "tag acc" }, d)),
          info.is_repo ? h("span", { class: "tag ok" }, icon("git"), info.branch || "detached HEAD") : h("span", { class: "tag" }, "not a Git repository")),
        info.remote_url ? h("div", { class: "mono", style: "margin-top:6px" }, info.remote_url) : null,
        info.head_summary ? h("div", { style: "margin-top:2px" }, "Latest commit: " + info.head_summary) : null,
        h("div", { class: "rowf", style: "margin-top:8px" }, info.scripts.length ? ["Scripts:", info.scripts.map((sc) => btn(sc, () => { f.script.value = sc; }, "sm"))]
          : "No " + (win ? ".bat / .cmd" : ".sh") + " scripts in this folder yet. Create one, then enter its name below.")));
    }
    f.dir.oninput = () => { clearTimeout(timer); timer = setTimeout(inspect, 350); };
    if (src) inspect();
    async function submit(e) {
      e.preventDefault(); err.textContent = "";
      const body = { name: f.name.value, working_directory: f.dir.value, script_name: f.script.value, git_branch: f.branch.value, run_mode: f.mode.value,
        health_url: f.health.value, description: f.desc.value, project: f.project.value };
      if (src) Object.assign(body, { env_vars: src.env_vars, schedule: src.schedule || "", timeout_secs: src.timeout_secs, auto_restart: src.auto_restart });
      const extra = workers.filter((x) => x.box.checked).map((x) => x.w);
      // Companions are grouped with the main process: give them a real project.
      if (extra.length && (!body.project.trim() || body.project.trim() === "Default")) body.project = body.name.trim();
      let t;
      try { t = await api("POST", "/api/targets", body); } catch (ex) { err.textContent = ex.message; return; }
      const failed = [];
      for (const w of extra) { try { await addWorker(t, w); } catch (ex) { failed.push(w.label + ": " + ex.message); } }
      if (failed.length) toast("Target created, but " + failed.join("; "), true);
      else toast(extra.length ? "Target and " + extra.length + " related process(es) created" : "Target created");
      await refresh();
      if (extra.length && !failed.length) location.hash = projHref(t.project); else go("target/" + t.id);
    }
    return h("div", { style: "max-width:760px" },
      h("div", { class: "crumbs" }, h("a", { href: "#/projects" }, "Projects"), icon("chevron"), "New target"),
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, src ? "Clone target" : "New target"),
        h("p", { class: "sub" }, "PortalControl runs the script in place. The folder is not copied, wrapped or modified."))),
      h("form", { onsubmit: submit, class: "box" },
        field("Working directory", f.dir, "Absolute path of the project on this machine."), found,
        h("div", { class: "cols" }, field("Name", f.name), field("Project", f.project, "Targets with the same project are grouped together.")),
        h("div", { class: "cols" }, field("Script", f.script, "A " + (win ? ".bat or .cmd" : ".sh") + " file inside the directory, run through " + (sys ? sys.shell : "the shell") + "."), field("Git branch to track", f.branch)),
        field("Run mode", f.mode, "Tasks are killed at their timeout (300 s by default). Services stay up until stopped."),
        field("Health URL", f.health, "Optional. Probed every 15 seconds to report LIVE / SLOW / DOWN."),
        field("Description", f.desc), workerBox,
        h("datalist", { id: "projlist" }, [...new Set(targets.map((t) => t.project))].map((p) => h("option", { value: p }))),
        h("div", { class: "formfoot" }, h("button", { class: "btn hl", type: "submit" }, "Create target"), h("a", { class: "btn", href: "#/projects" }, "Cancel"), err)),
      h("p", { class: "hint", style: "margin-top:12px" }, "Environment variables, schedules, auto-deploy and webhooks are configured on the target's page after it is created."));
  };
  return def;
}

