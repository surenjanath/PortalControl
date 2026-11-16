"use strict";
// ------------------------------------------------------------- new target --
// A guided form. Point it at a folder and it works out the project type,
// lists the scripts it found, and offers to write a start script (asking for
// a port) when the project has none. Everything it prefills can be changed.

// Folder picker: browse this machine's folders instead of typing a path.
function pickFolder(startAt, onPick) {
  const title = h("div", { class: "mono", style: "word-break:break-all;font-size:12.5px" });
  const list = h("div", { class: "picklist" });
  const roots = h("div", { class: "rowf", style: "margin-bottom:8px" });
  const use = h("button", { class: "btn hl", type: "button" }, "Use this folder");
  const dlg = h("dialog", { style: "width:min(640px,94vw)" },
    h("div", { class: "dh" }, "Choose the project folder"),
    h("div", { class: "db", style: "color:var(--text)" }, roots, title, list),
    h("div", { class: "df" }, h("button", { class: "btn", type: "button", onclick: () => close() }, "Cancel"), use));
  const close = () => { dlg.close(); dlg.remove(); };
  let current = null;
  const open = async (dir) => {
    let d;
    try { d = await api("GET", "/api/fs/list?dir=" + encodeURIComponent(dir || "")); } catch (e) { return toast(e.message, true); }
    if (d.error) { if (dir) return open(""); return toast(d.error, true); }
    current = d.dir;
    title.replaceChildren(d.dir, d.is_project ? h("span", { class: "tag acc", style: "margin-left:8px" }, "looks like a project") : null);
    roots.replaceChildren(d.home ? btn("Home", () => open(d.home), "sm", "home") : null, d.roots.map((r) => btn(r, () => open(r), "sm")),
      d.parent ? btn("Up one level", () => open(d.parent), "sm right") : null);
    list.replaceChildren(d.entries.length ? d.entries.map((e) => h("div", { class: "pick", tabindex: "0", role: "button", onclick: () => open(e.path),
        onkeydown: (ev) => { if (ev.key === "Enter") open(e.path); } },
      icon("box"), h("span", { style: "flex:1;overflow:hidden;text-overflow:ellipsis" }, e.name), e.project ? h("span", { class: "tag acc" }, "project") : null, icon("chevron")))
      : h("div", { class: "muted small", style: "padding:14px" }, "No sub-folders here."));
    list.scrollTop = 0;
  };
  use.onclick = () => { if (current) { close(); onPick(current); } };
  dlg.addEventListener("cancel", (e) => { e.preventDefault(); close(); });
  document.body.append(dlg);
  dlg.showModal();
  open(startAt);
}

function newView(cloneId) {
  const def = { key: "new/" + (cloneId || "") };
  // The server's OS decides path and script examples. Until /api/system has
  // answered, guess from the browser, then correct the placeholders once it does.
  const isWin = () => (sys ? sys.os === "windows" : (/^win/i.test(navigator.platform || "") || /windows/i.test(navigator.userAgent || "")));
  const examples = () => (isWin() ? { dir: "C:\\Projects\\my_app", script: "deploy.bat" } : { dir: "/Users/me/projects/my_app", script: "deploy.sh" });
  // The form must survive background data refreshes, so only the placeholders are touched.
  let placeholders = null;
  def.update = () => { if (placeholders) placeholders(); };
  def.build = () => {
    const src = cloneId ? T(cloneId) : null;
    const text = (attrs) => h("input", Object.assign({ type: "text", spellcheck: "false", autocomplete: "off" }, attrs));
    const f = {
      dir: text({ value: src ? src.working_directory : "", placeholder: examples().dir }),
      name: text({ maxlength: "100", value: src ? src.name + " (copy)" : "", placeholder: "Claims Portal", spellcheck: "true" }),
      project: text({ maxlength: "60", value: src ? src.project : "Default", list: "projlist" }),
      branch: text({ value: src ? src.git_branch : "main" }),
      mode: h("select", {}, h("option", { value: "TASK" }, "Task \u2014 runs to completion (a deployment script)"), h("option", { value: "SERVICE" }, "Service \u2014 long-running process (a server)")),
      health: text({ value: src && src.health_url ? src.health_url : "", placeholder: "http://127.0.0.1:8000/" }),
      desc: text({ maxlength: "500", value: src ? src.description : "", placeholder: "Optional", spellcheck: "true" }),
      custom: text({ placeholder: examples().script, value: src ? src.script_name : "" }),
      port: h("input", { type: "number", min: "1", max: "65535", style: "max-width:140px" }),
      lan: h("input", { type: "checkbox" }),
      backup: h("input", { type: "checkbox", checked: true }),
      autostart: h("input", { type: "checkbox", checked: !!(src && src.autostart) }),
      restart: h("input", { type: "checkbox", checked: src ? src.auto_restart : true }),
    };
    if (src) f.mode.value = src.run_mode;
    placeholders = () => { const ex = examples(); f.dir.placeholder = ex.dir; f.custom.placeholder = ex.script; };
    const touched = { name: !!src, branch: !!src, health: !!src, mode: !!src };
    f.name.oninput = () => (touched.name = true);
    f.branch.oninput = () => (touched.branch = true);
    f.health.oninput = () => (touched.health = true);
    f.mode.onchange = () => { touched.mode = true; paintOptions(); };

    let info = null;                     // result of inspecting the folder
    let choice = src ? { type: "custom" } : null;   // {type:"script",script} | {type:"starter",id} | {type:"custom"}
    let workers = [];
    let timer = null, seq = 0, portSeq = 0;
    const found = h("div"), runBox = h("div"), portBox = h("div"), optionsBox = h("div"), rest = h("div", { style: "display:none" });

    // ---- step 1, "Clone from Git": clone into a folder, then carry on as with a local folder ----
    const g = {
      url: text({ placeholder: "https://github.com/acme/shop.git", "aria-label": "Repository URL" }),
      parent: text({ placeholder: isWin() ? "C:\\Projects" : "/Users/me/projects", "aria-label": "Clone into this folder" }),
      name: text({ placeholder: "folder name", "aria-label": "New folder name" }),
      branch: text({ placeholder: "default branch", "aria-label": "Branch" }),
      token: h("input", { type: "password", autocomplete: "off", placeholder: "access token (only for a private repository)", "aria-label": "Access token" }),
    };
    let nameTouched = false, savedHosts = [];
    const cloneMsg = h("div", { class: "muted small", style: "min-height:18px" });
    const tokenRow = h("div");
    const hostOf = (u) => { const m = /^https?:\/\/(?:[^@/]+@)?([^/:]+)/i.exec(u.trim()); return m ? m[1].toLowerCase() : ""; };
    const paintToken = () => {
      const host = hostOf(g.url.value);
      tokenRow.replaceChildren(!host ? null : savedHosts.includes(host)
        ? h("div", { class: "muted small" }, icon("shield"), " An access token for " + host + " is saved; it is used for private repositories.")
        : field("Private repository? Access token for " + host, g.token, "Optional. Saved encrypted for " + host + " and used for later pulls too. GitHub: a fine-grained token with read access to contents."));
    };
    api("GET", "/api/git/tokens").then((r) => { savedHosts = r.hosts; paintToken(); }, () => {});
    g.name.oninput = () => (nameTouched = true);
    g.url.oninput = () => {
      if (!nameTouched) { const last = g.url.value.trim().replace(/\/+$/, "").split(/[\/:]/).pop() || ""; g.name.value = last.replace(/\.git$/, ""); }
      paintToken();
    };
    const cloneBtn = btn("Clone", async () => {
      cloneBtn.disabled = true;
      const host = hostOf(g.url.value);
      try {
        if (g.token.value.trim()) { await api("PUT", "/api/git/tokens", { host, token: g.token.value.trim() }); g.token.value = ""; savedHosts.push(host); }
        cloneMsg.className = "small"; cloneMsg.replaceChildren(h("span", { class: "dot info pulse" }), " Cloning\u2026");
        const r = await api("POST", "/api/git/clone", { url: g.url.value.trim(), parent: g.parent.value.trim(), name: g.name.value.trim(), branch: g.branch.value.trim() });
        cloneMsg.className = "small"; cloneMsg.replaceChildren(h("span", { class: "dot ok" }), " Cloned into ", h("span", { class: "mono" }, r.dir), r.branch ? " (" + r.branch + ")" : "");
        f.dir.value = r.dir; mode("folder", true); inspect();
      } catch (e) { cloneMsg.className = "small t-err"; cloneMsg.textContent = e.message; }
      cloneBtn.disabled = false;
    }, "hl", "download");
    const cloneSink = (m) => {
      if (m.type !== "clone_progress" || !cloneBtn.disabled) return;
      const pct = m.total ? Math.round((m.received / m.total) * 100) : 0;
      cloneMsg.className = "small"; cloneMsg.replaceChildren(h("span", { class: "dot info pulse" }), " Cloning\u2026 " + pct + "% (" + m.received + " of " + m.total + " objects)");
    };
    sinks.add(cloneSink);
    def.cleanup = () => sinks.delete(cloneSink);
    const clonePanel = h("div", {},
      h("div", { class: "grid g2", style: "gap:10px" }, field("Repository URL (HTTPS)", g.url), field("Branch", g.branch, "Leave empty for the repository's default.")),
      h("div", { class: "grid g2", style: "gap:10px" },
        field("Clone into", h("div", { class: "rowf", style: "flex-wrap:nowrap" }, h("div", { style: "flex:1" }, g.parent),
          btn("Browse\u2026", () => pickFolder(g.parent.value.trim(), (p) => { g.parent.value = p; }), "", "box"))),
        field("New folder", g.name, "Created inside that folder; it must not exist yet.")),
      tokenRow, h("div", { class: "rowf", style: "margin-top:6px" }, cloneBtn, cloneMsg));
    const folderPanel = h("div");
    const tabs = h("div", { class: "rowf", style: "margin:0 0 12px" });
    const mode = (which, keep) => {
      tabs.replaceChildren(btn("Folder on this machine", () => mode("folder"), "sm" + (which === "folder" ? " on" : ""), "box"),
        btn("Clone from Git", () => mode("clone"), "sm" + (which === "clone" ? " on" : ""), "download"));
      folderPanel.style.display = which === "folder" ? "" : "none";
      clonePanel.style.display = which === "clone" ? "" : "none";
      if (!keep && which === "clone") g.url.focus();
    };
    mode("folder", true);
    const err = h("span", { class: "errtxt" });
    const starter = () => (choice && choice.type === "starter" && info ? info.starters.find((s) => s.id === choice.id) : null);
    const isService = () => (starter() ? starter().run_mode === "SERVICE" : f.mode.value === "SERVICE");

    async function checkPort() {
      const st = starter();
      if (!st || !st.default_port) return portBox.replaceChildren();
      const port = parseInt(f.port.value, 10);
      if (!port) return;
      if (!touched.health) f.health.value = "http://127.0.0.1:" + port + "/";
      const mine = ++portSeq;
      let r; try { r = await api("GET", "/api/fs/port?port=" + port); } catch (_) { return; }
      if (mine !== portSeq) return;
      const note = $("port-note"); if (!note) return;
      const useIt = r.suggestion ? btn("Use " + r.suggestion, () => { f.port.value = r.suggestion; checkPort(); }, "sm") : null;
      note.replaceChildren(r.used_by ? [h("span", { class: "errtxt" }, "Port " + port + " is already used by \u201c" + r.used_by + "\u201d. "), useIt]
        : r.in_use ? [h("span", { style: "color:var(--warn)" }, "Something is listening on port " + port + " right now. If that is this project running outside PortalControl, stop it first. "), useIt]
        : h("span", { style: "color:var(--ok)" }, "Port " + port + " is free."));
    }
    function paintPort() {
      const st = starter();
      if (!st || !st.default_port) return portBox.replaceChildren();
      if (!f.port.value) f.port.value = st.default_port;
      portBox.replaceChildren(h("div", { class: "box", style: "margin:4px 0 14px;background:var(--bg)" },
        h("div", { class: "rowf", style: "align-items:flex-end" }, h("label", { class: "f", style: "margin:0" }, h("span", {}, "Port"), f.port),
          h("div", { id: "port-note", class: "small", style: "flex:1;min-width:220px;padding-bottom:8px" })),
        h("label", { class: "check", style: "margin:12px 0 0" }, f.lan, h("span", {}, h("b", {}, "Reachable from other machines"),
          h("small", {}, "Listen on every network interface (0.0.0.0) instead of this machine only (127.0.0.1). Needed for colleagues to open it; check the firewall and the app's own allowed hosts."))),
        h("div", { class: "hint", style: "margin-top:10px" }, "PortalControl writes ", h("span", { class: "mono" }, st.script), " into the project folder", st.script_exists && st.script_is_ours ? " (replacing the one it wrote before)" : "", " with:"),
        h("pre", { class: "mono", style: "margin:4px 0 0;white-space:pre-wrap;word-break:break-all;color:var(--muted)" },
          st.commands.map((c) => c.replace("{host}", f.lan.checked ? "0.0.0.0" : "127.0.0.1").replace("{port}", f.port.value || st.default_port)).join("\n"))));
      checkPort();
    }
    // Repainting re-inserts the port field, so put the cursor back if it was there.
    f.port.oninput = () => { clearTimeout(timer); timer = setTimeout(() => { const typing = document.activeElement === f.port; paintPort(); if (typing) f.port.focus(); }, 400); };
    f.lan.onchange = paintPort;

    function pick(c) {
      choice = c;
      const st = starter();
      if (st) { f.mode.value = st.run_mode; f.mode.disabled = true; f.port.value = ""; if (!st.default_port && !touched.health) f.health.value = ""; }
      else {
        f.mode.disabled = false;
        const name = c.type === "script" ? c.script : f.custom.value;
        if (!touched.mode) f.mode.value = /run|start|serve|server|web|worker|cluster/i.test(name) ? "SERVICE" : "TASK";
      }
      paintRun(); paintOptions();
    }
    function option(c, on, title, tags, detail) {
      return h("label", { class: "choice" + (on ? " on" : "") },
        h("input", { type: "radio", name: "how", checked: on, onchange: () => pick(c) }),
        h("span", { style: "flex:1;min-width:0" }, h("span", { class: "rowf" }, h("b", {}, title), tags), detail ? h("small", {}, detail) : null));
    }
    function paintRun() {
      if (!info) return runBox.replaceChildren();
      const ours = new Set(info.starters.filter((s) => s.script_is_ours).map((s) => s.script.toLowerCase()));
      const userScripts = info.scripts.filter((s) => !ours.has(s.toLowerCase()) && !/^portalcontrol_/i.test(s));
      const helperScripts = info.scripts.filter((s) => !userScripts.includes(s) && !ours.has(s.toLowerCase()));
      runBox.replaceChildren(
        userScripts.length ? [h("div", { class: "optlabel" }, "Scripts found in the folder"),
          userScripts.map((s) => option({ type: "script", script: s }, choice && choice.type === "script" && choice.script === s, s, null, "Run this script as it is."))] : null,
        info.starters.length ? [h("div", { class: "optlabel" }, userScripts.length ? "Or let PortalControl write a start script" : "This project has no script yet. PortalControl can write one"),
          info.starters.map((s) => option({ type: "starter", id: s.id }, choice && choice.type === "starter" && choice.id === s.id, s.label,
            [h("span", { class: "tag" }, s.run_mode === "SERVICE" ? "Service" : "Task"), s.script_exists && s.script_is_ours ? h("span", { class: "tag acc" }, "already generated") : null,
              s.script_exists && !s.script_is_ours ? h("span", { class: "tag warn" }, s.script + " exists") : null], s.description)), portBox] : null,
        helperScripts.length ? [h("div", { class: "optlabel" }, "Launchers written earlier"),
          helperScripts.map((s) => option({ type: "script", script: s }, choice && choice.type === "script" && choice.script === s, s, null, null))] : null,
        h("div", { class: "optlabel" }, "Something else"),
        option({ type: "custom" }, choice && choice.type === "custom", "Another script", null, "Type the name of a " + (isWin() ? ".bat or .cmd" : ".sh") + " file inside the folder."),
        choice && choice.type === "custom" ? h("div", { style: "margin:4px 0 12px 30px;max-width:360px" }, f.custom) : null);
      paintPort();
    }
    function paintOptions() {
      if (!info) return optionsBox.replaceChildren();
      const db = (info.sqlite_files || [])[0];
      optionsBox.replaceChildren(
        workers.length ? [h("div", { class: "optlabel", style: "margin-top:0" }, "Related processes found"),
          workers.map(({ w, box }) => (w.started_by || []).length
            ? check(box, "Also add: " + w.label, "Not ticked: " + w.started_by.join(", ") + " already starts it. Tick this only if the script you run does not, or the same process runs twice on one database.")
            : check(box, "Also add: " + w.label, w.command + (w.script_exists ? "" : "  \u2014 creates " + w.script + " in the folder")))] : null,
        db ? check(f.backup, "Back up " + db + " before every run", "Keeps the newest 14 snapshots of the SQLite database in PortalControl's data folder, so a bad deployment or migration can be undone.") : null,
        isService() ? check(f.restart, "Restart on crash", "If the process exits on its own it is started again after 5 seconds.") : null,
        check(f.autostart, "Start with PortalControl", "Launch it whenever the manager starts, for example after a reboot."));
    }

    async function inspect() {
      const dir = f.dir.value.trim().replace(/^"|"$/g, "");
      if (!dir) { info = null; rest.style.display = "none"; return found.replaceChildren(); }
      const mine = ++seq;
      let r;
      try { r = await api("GET", "/api/fs/inspect?dir=" + encodeURIComponent(dir)); } catch (e) { return; }
      if (mine !== seq) return;
      if (!r.exists) { info = null; rest.style.display = "none"; return found.replaceChildren(h("div", { class: "note-box err" }, r.error || "That folder was not found on this machine.")); }
      info = r;
      rest.style.display = "";
      if (!touched.name && r.suggested_name) f.name.value = r.suggested_name.replace(/[_-]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
      if (!touched.branch && r.branch) f.branch.value = r.branch;
      const registered = targets.filter((o) => o.working_directory.toLowerCase() === dir.toLowerCase());
      const taken = registered.map((o) => o.script_name.toLowerCase());
      workers = (r.workers || []).filter((w) => !taken.includes(w.script.toLowerCase())).map((w) => ({ w, box: h("input", { type: "checkbox", checked: !src && !(w.started_by || []).length }) }));
      found.replaceChildren(h("div", { class: "note-box ok" },
        h("div", { class: "rowf" }, h("b", { style: "color:var(--text);font-size:14px" }, r.project.label), r.detected.filter((d) => d !== r.project.label).map((d) => h("span", { class: "tag acc" }, d)),
          r.is_repo ? h("span", { class: "tag ok" }, icon("git"), r.branch || "detached HEAD") : h("span", { class: "tag" }, "not a Git repository")),
        r.remote_url ? h("div", { class: "mono", style: "margin-top:6px" }, r.remote_url) : null,
        r.head_summary ? h("div", { style: "margin-top:2px" }, "Latest commit: " + r.head_summary) : null,
        registered.length ? h("div", { style: "margin-top:6px;color:var(--warn)" }, "Already registered from this folder: " + registered.map((o) => o.name + " (" + o.script_name + ")").join(", ") + ". Add another target only for a different process.") : null));
      if (!src) {
        // Sensible default: the project's own script if there is exactly one, else the first generated option.
        const own = r.scripts.filter((s) => !/^portalcontrol_/i.test(s) && !taken.includes(s.toLowerCase()));
        const firstStarter = r.starters.find((s) => !taken.includes(s.script.toLowerCase()) && (!s.script_exists || s.script_is_ours));
        pick(own.length === 1 ? { type: "script", script: own[0] } : firstStarter ? { type: "starter", id: firstStarter.id } : own.length ? { type: "script", script: own[0] } : { type: "custom" });
      } else { paintRun(); paintOptions(); }
    }
    f.dir.oninput = () => { clearTimeout(timer); timer = setTimeout(inspect, 350); };
    if (src) inspect();

    async function submit(e) {
      e.preventDefault(); err.textContent = "";
      if (!info) { err.textContent = "Choose the project folder first"; return; }
      const dir = f.dir.value.trim().replace(/^"|"$/g, "");
      const save = $("create-btn"); save.disabled = true;
      try {
        let script = choice && choice.type === "script" ? choice.script : f.custom.value.trim();
        const st = starter();
        if (st) {
          const made = await api("POST", "/api/fs/starter", { dir, starter: st.id, host: f.lan.checked ? "0.0.0.0" : "127.0.0.1", port: st.default_port ? parseInt(f.port.value, 10) || null : null });
          script = made.script;
        }
        if (!script) throw new Error("Choose how the project is started");
        const db = (info.sqlite_files || [])[0];
        const body = { name: f.name.value, working_directory: dir, script_name: script, git_branch: f.branch.value, run_mode: st ? st.run_mode : f.mode.value,
          health_url: f.health.value, description: f.desc.value, project: f.project.value, autostart: f.autostart.checked,
          auto_restart: isService() && f.restart.checked, db_file: db && f.backup.checked ? db : "", backup_on_deploy: !!(db && f.backup.checked),
          python_env: src ? src.python_env || "" : "auto" };
        if (src) Object.assign(body, { env_vars: src.env_vars, schedule: src.schedule || "", timeout_secs: src.timeout_secs });
        const extra = workers.filter((x) => x.box.checked).map((x) => x.w);
        // Companions are grouped with the main process: give them a real project.
        if (extra.length && (!body.project.trim() || body.project.trim() === "Default")) body.project = body.name.trim();
        const t = await api("POST", "/api/targets", body);
        const failed = [];
        for (const w of extra) { try { await addWorker(t, w); } catch (ex) { failed.push(w.label + ": " + ex.message); } }
        if (failed.length) toast("Target created, but " + failed.join("; "), true);
        else toast(extra.length ? "Target and " + extra.length + " related process(es) created" : "Target created");
        await refresh();
        if (extra.length && !failed.length) location.hash = projHref(t.project); else go("target/" + t.id);
      } catch (ex) { err.textContent = ex.message; save.disabled = false; }
    }

    const step = (n, title, sub) => h("div", { class: "step" }, h("span", { class: "stepn" }, n), h("div", {}, h("b", {}, title), sub ? h("div", { class: "muted small" }, sub) : null));
    return h("div", { style: "max-width:820px" },
      h("div", { class: "crumbs" }, h("a", { href: "#/projects" }, "Projects"), icon("chevron"), "New target"),
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, src ? "Add another process" : "New target"),
        h("p", { class: "sub" }, "Point PortalControl at a project folder. It runs the project in place: nothing is copied or wrapped."))),
      h("form", { onsubmit: submit },
        h("div", { class: "box", style: "margin-bottom:14px" }, step(1, "Project folder", "A folder on this machine, or a Git repository to clone into one."),
          src ? null : tabs, src ? null : clonePanel,
          (folderPanel.replaceChildren(h("div", { class: "rowf", style: "flex-wrap:nowrap;margin-bottom:10px" }, h("div", { style: "flex:1" }, f.dir),
            btn("Browse\u2026", () => pickFolder(f.dir.value.trim(), (p) => { f.dir.value = p; inspect(); }), "", "box"))), folderPanel), found),
        h("div", {}, rest),
        h("datalist", { id: "projlist" }, [...new Set(targets.map((t) => t.project))].map((p) => h("option", { value: p })))),
      (() => {
        rest.replaceChildren(
          h("div", { class: "box", style: "margin-bottom:14px" }, step(2, "How it runs", "Pick the script that starts or deploys it, or let PortalControl write one."), runBox),
          h("div", { class: "box", style: "margin-bottom:14px" }, step(3, "Details", "Prefilled from the folder; change anything."),
            h("div", { class: "cols" }, field("Name", f.name), field("Project", f.project, "Targets with the same project are grouped and started together.")),
            h("div", { class: "cols" }, field("Run mode", f.mode, "Tasks are stopped at their timeout (300 s by default). Services stay up until stopped."), field("Git branch to track", f.branch)),
            field("Health URL", f.health, "Optional. Probed every 15 seconds to report LIVE / SLOW / DOWN. Filled in from the port when PortalControl writes the start script."),
            field("Description", f.desc)),
          h("div", { class: "box", style: "margin-bottom:14px" }, step(4, "Options"), optionsBox),
          h("div", { class: "formfoot" }, h("button", { class: "btn hl", type: "submit", id: "create-btn" }, "Create target"), h("a", { class: "btn", href: "#/projects" }, "Cancel"), err),
          h("p", { class: "hint", style: "margin-top:12px" }, "Environment variables, schedules, auto-deploy and webhooks are set on the target's page after it is created."));
        return null;
      })());
  };
  return def;
}
