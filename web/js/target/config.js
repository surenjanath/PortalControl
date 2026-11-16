"use strict";
// ---- configuration ----
// Each section of the Configuration tab is its own function. CFG.<name>(ctx)
// returns the content of that section; ctx = { t, id, live, onUpdate }:
//   t        the target as it was when the section was opened
//   live(fn) a block that is repainted with fresh data on every refresh
//   onUpdate optional callback the section sets to react to refreshes
const CFG = {};

CFG.general = (ctx) => {
  const { t, id, live } = ctx;
    const f = {
      name: h("input", { type: "text", maxlength: "100", value: t.name }), desc: h("input", { type: "text", maxlength: "500", value: t.description, placeholder: "What this target is for" }),
      project: h("input", { type: "text", maxlength: "60", value: t.project, list: "projlist2" }),
      dir: h("input", { type: "text", value: t.working_directory, spellcheck: "false" }), script: h("input", { type: "text", value: t.script_name, spellcheck: "false" }),
      mode: h("select", {}, h("option", { value: "TASK" }, "Task \u2014 runs to completion"), h("option", { value: "SERVICE" }, "Service \u2014 long-running")),
    };
    f.mode.value = t.run_mode;
    // Script picker: the other scripts in the folder, and start scripts
    // PortalControl can write for this project type. Picking one only fills
    // in the form; nothing changes until Save.
    const picker = h("div", { class: "hint", style: "margin:-6px 0 14px" }, "Looking for scripts in the folder…");
    let pendingHealth = null;
    const currentPort = (() => { try { return t.health_url ? new URL(t.health_url).port : ""; } catch (_) { return ""; } })();
    const useScript = (name) => { f.script.value = name; paintPicker(); };
    let found = null;
    const paintPicker = () => {
      if (!found || !found.exists) return picker.replaceChildren(found ? "The working directory could not be read." : "Looking for scripts in the folder…");
      const chosen = f.script.value.trim().toLowerCase();
      const known = found.scripts.some((x) => x.toLowerCase() === chosen);
      picker.replaceChildren(
        h("div", { class: "rowf" }, h("span", {}, found.project.label + " project. Scripts in the folder:"),
          found.scripts.length ? found.scripts.map((name) => btn(name, () => useScript(name), "sm" + (name.toLowerCase() === chosen ? " on" : ""))) : h("span", {}, "none")),
        !known && chosen ? h("div", { class: "errtxt", style: "margin-top:6px" }, "“" + f.script.value + "” is not in the folder.") : null,
        found.starters.length ? h("div", { style: "margin-top:10px" }, h("div", { style: "margin-bottom:6px" }, "Or have PortalControl write a start script for this project:"),
          found.starters.map((st) => {
            const port = st.default_port ? h("input", { type: "number", min: "1", max: "65535", value: currentPort || st.default_port, style: "width:96px;padding:3px 8px", "aria-label": "Port for " + st.label }) : null;
            const blocked = st.script_exists && !st.script_is_ours;
            return h("div", { class: "rowf", style: "margin-bottom:6px" }, h("span", { style: "min-width:230px;color:var(--text)" }, st.label), port ? ["port", port] : h("span", { class: "dim" }, "no port needed"),
              btn(st.script_exists && st.script_is_ours ? "Rewrite & use" : "Write & use", async () => {
                try {
                  const made = await api("POST", "/api/fs/starter", { dir: f.dir.value.trim(), starter: st.id, host: "127.0.0.1", port: port ? parseInt(port.value, 10) || null : null });
                  f.script.value = made.script; f.mode.value = st.run_mode; pendingHealth = made.health_url;
                  toast(made.script + " written. Press Save to switch to it" + (made.health_url ? " (the health URL is updated too)." : "."));
                  load();
                } catch (e) { toast(e.message, true); }
              }, "sm", null, { disabled: blocked, title: blocked ? st.script + " exists and was not written by PortalControl; pick it from the list above instead" : st.description }));
          })) : null);
    };
    const load = () => api("GET", "/api/fs/inspect?dir=" + encodeURIComponent(f.dir.value.trim())).then((r) => { found = r; paintPicker(); }, () => {});
    load();
    f.script.addEventListener("input", paintPicker);
    f.dir.addEventListener("change", load);
    return [
      live((x) => h("div", { class: "box", style: "margin-bottom:22px" }, facts([
        ["Status", pill(tStatus(x))],
        x.running ? ["Process", "pid " + x.running.pid + " \u00b7 up " + fmtUp(x.running.uptime_secs) + " \u00b7 " + trig(x.running.triggered_by).toLowerCase() + (x.metrics ? " \u00b7 " + x.metrics.cpu.toFixed(1) + "% CPU \u00b7 " + fmtBytes(x.metrics.mem) + " \u00b7 " + x.metrics.procs + " process(es)" : "")] : null,
        ["Last deployment", x.last_run ? h("span", {}, runTag(x.last_run.status), " ", h("a", { href: "#/target/" + id + "/deployments/" + x.last_run.id, style: "text-decoration:underline" }, "#" + x.last_run.id), " \u00b7 ", when(parseUTC(x.last_run.executed_at)), " \u00b7 " + fmtDur(x.last_run.duration_ms) + " \u00b7 " + trig(x.last_run.triggered_by)) : "never"],
        x.git && x.git.is_repo ? ["Commit", h("span", {}, h("span", { class: "mono" }, short(x.git.head)), " ", x.git.head_summary || "", x.git.head_author ? h("span", { class: "dim" }, " \u2014 " + x.git.head_author) : null)] : null,
        x.health_url ? ["Health", h("span", { class: "rowf" }, x.health ? x.health.status + (x.health.latency_ms !== null ? " \u00b7 " + x.health.latency_ms + " ms" : "") : "checking", strip(x, 30), x.uptime_pct !== null ? x.uptime_pct + "% reachable" : "")] : null,
        x.schedule ? ["Next scheduled run", x.next_run ? h("span", {}, fmtDT(new Date(x.next_run * 1000)), h("span", { class: "dim" }, "  (" + fmtAgo(new Date(x.next_run * 1000)) + ")")) : "never"] : null,
        ["Created", fmtDT(parseUTC(x.created_at))],
      ]))),
      saveForm(t, [
        h("div", { class: "cols" }, field("Name", f.name), field("Project", f.project, "Used to group targets.")),
        field("Description", f.desc),
        field("Working directory", f.dir, "Absolute path on this machine. The folder is used in place."),
        h("div", { class: "cols" }, field("Script", f.script, "Run through " + (sys ? sys.shell : "the host shell") + " with the working directory as current directory."), field("Run mode", f.mode)),
        picker,
        h("datalist", { id: "projlist2" }, [...new Set(targets.map((x) => x.project))].map((p) => h("option", { value: p }))),
      ], () => Object.assign({ name: f.name.value, description: f.desc.value, project: f.project.value, working_directory: f.dir.value, script_name: f.script.value, run_mode: f.mode.value },
        pendingHealth ? { health_url: pendingHealth } : {})),
      h("div", { style: "margin-top:18px" }, h("a", { class: "btn", href: "#/new/" + id }, icon("copy"), "Clone this target")),
    ];
};

CFG.processes = (ctx) => {
  const { t, id, live } = ctx;
    let found = null;
    const detected = h("div");
    const paintDetected = () => {
      const x = T(id); if (!x) return;
      if (!found) return detected.replaceChildren(h("p", { class: "muted" }, "Looking at the project folder…"));
      const sameDir = targets.filter((o) => o.working_directory === x.working_directory);
      const ws = found.workers || [];
      detected.replaceChildren(ws.length ? ws.map((w) => {
        const reg = sameDir.find((o) => o.script_name.toLowerCase() === w.script.toLowerCase());
        return h("div", { class: "box rowf", style: "margin-bottom:8px" },
          h("div", { style: "flex:1;min-width:260px" }, h("b", {}, w.label), reg ? [" ", pill(tStatus(reg))] : null,
            h("div", { class: "mono muted", style: "margin-top:4px;word-break:break-all" }, w.command),
            h("div", { class: "dim small", style: "margin-top:2px" }, "Detected: " + w.why + (w.script_exists ? " · launcher " + w.script + " exists" : "")),
            (w.started_by || []).length ? h("div", { class: "small t-warn", style: "margin-top:2px" }, (reg ? "Runs twice if " : "Already started by ") + w.started_by.join(", ") + (reg ? " is also running: that script starts it too." : ". Adding it would run it twice.")) : null),
          reg ? h("a", { class: "btn sm", href: "#/target/" + reg.id }, "Open") : btn("Add to project", async () => {
            const project = x.project === "Default" ? x.name : x.project;
            const body = [h("p", {}, "This registers “" + x.name + " · " + w.label + "” as a second service in project “" + project + "”, with restart on crash enabled."),
              (w.started_by || []).length ? h("p", { class: "t-warn" }, w.started_by.join(", ") + " already starts the " + w.label + ". If a target runs that script, the process will run twice against the same database. Remove it from the script first, or do not add this.") : null,
              w.script_exists ? null : h("p", {}, "A small launcher script, ", h("span", { class: "mono" }, w.script), ", will be created in the project folder. It only runs: ", h("span", { class: "mono" }, w.command)),
              project !== x.project ? h("p", {}, "“" + x.name + "” moves from the Default project into “" + project + "” so the two are grouped.") : null];
            if (!(await confirmDlg("Add " + w.label + "?", body, "Add process"))) return;
            try {
              await addWorker(x, w, project);
              if (project !== x.project) await saveTarget(x, { project }, "Added " + w.label); else { toast("Added " + w.label); await refresh(true); }
            } catch (e) { toast(e.message, true); }
          }, "hl sm", "plus"));
      }) : h("div", { class: "note-box" }, found.exists === false ? "The working directory could not be inspected." :
        "No background workers were detected here. PortalControl recognises Django-Q (django_q in settings or requirements) and Celery (a celery.py next to settings.py). Any other process can be added by hand with the button below."));
    };
    api("GET", "/api/fs/inspect?dir=" + encodeURIComponent(t.working_directory)).then((r) => { found = r; paintDetected(); }, () => { found = { exists: false }; paintDetected(); });
    ctx.onUpdate = paintDetected;
    return [
      h("div", { class: "note-box" }, "A project is often more than one process: a web server plus a queue cluster or workers. Each one is its own target with its own logs, health and restart policy; targets that share a project are started, stopped and watched together on the project page."),
      live((x) => {
        const mates = targets.filter((o) => o.project === x.project && o.id !== x.id);
        return [h("h3", { style: "margin-top:0" }, "In project “" + x.project + "”", h("a", { class: "btn sm", href: projHref(x.project), style: "margin-left:12px;font-weight:500" }, "Open project terminals")),
          mates.length ? table(["Process", "Status", "Script", ""], mates.map((o) => [h("a", { href: "#/target/" + o.id, style: "font-weight:600" }, o.name), pill(tStatus(o)),
            h("span", { class: "mono" }, o.script_name), h("div", { class: "rowf", style: "justify-content:flex-end;flex-wrap:nowrap" }, actionButtons(o, true))]))
            : h("div", { class: "muted small" }, "This is the only process in the project so far.")];
      }),
      h("h3", {}, "Detected in this folder"), detected,
      h("div", { style: "margin-top:16px" }, h("a", { class: "btn", href: "#/new/" + id }, icon("plus"), "Add another process by hand")),
    ];
};

CFG.environment = (ctx) => {
  const { t, id, live } = ctx;
    const ta = h("textarea", { spellcheck: "false", placeholder: "DJANGO_SETTINGS_MODULE=portal.settings\nDEBUG=0\n# one KEY=VALUE per line" });
    ta.value = t.env_vars;
    const list = h("div");
    let reveal = false;
    const paintList = () => {
      const rows = ta.value.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#") && l.includes("=")).map((l) => { const i = l.indexOf("="); return [l.slice(0, i).replace(/^export\s+/, "").trim(), l.slice(i + 1).trim().replace(/^(["'])(.*)\1$/, "$2")]; });
      list.replaceChildren(rows.length ? table(["Variable", h("span", { class: "rowf" }, "Value", btn(reveal ? "Hide" : "Reveal", () => { reveal = !reveal; paintList(); }, "ghost sm"))],
        rows.map(([k, v]) => [h("span", { class: "mono" }, k), h("span", { class: "mono" }, reveal ? v || h("span", { class: "dim" }, "(empty)") : "\u2022".repeat(Math.min(12, Math.max(4, v.length))))]))
        : h("div", { class: "muted small" }, "No variables defined."));
    };
    ta.oninput = paintList; paintList();
    return [
      h("div", { class: "note-box" }, "These variables are set for the script and everything it starts. They are stored in PortalControl's local database, not in the project folder. ",
        h("span", { class: "mono" }, "PORTALCONTROL=1"), " and ", h("span", { class: "mono" }, "PYTHONUNBUFFERED=1"), " are always set. Changes apply to the next run."),
      saveForm(t, [field("Variables (.env format)", ta, "One KEY=VALUE per line. Lines starting with # are ignored; quotes around a value are removed.")], () => ({ env_vars: ta.value }), "Environment variables saved"),
      h("h3", {}, "Parsed"), list,
    ];
};

CFG.git = (ctx) => {
  const { t, id, live } = ctx;
    const branch = h("input", { type: "text", value: t.git_branch, spellcheck: "false" });
    const auto = h("input", { type: "checkbox", checked: t.auto_deploy });
    const commits = h("div"), changes = h("div");
    const load = () => {
      api("GET", "/api/targets/" + id + "/git/log").then((log) => {
        commits.replaceChildren(log.length ? table(["", "Commit", "Message", "Author", "When", ""], log.map((cm) => [
          cm.state === "incoming" ? h("span", { class: "tag info" }, "incoming") : cm.state === "local" ? h("span", { class: "tag warn" }, "not pushed") : h("span", { class: "tag" }, "deployed"),
          h("span", { class: "mono" }, short(cm.sha)), cm.summary, cm.author, when(new Date(cm.time * 1000)),
          cm.state === "synced" ? rollbackButton(T(id), cm.sha, true) : null])) : h("div", { class: "muted small" }, "No commits to show."));
      }, () => {});
      api("GET", "/api/targets/" + id + "/git/changes").then((files) => {
        changes.replaceChildren(files.length ? table(["State", "Path"], files.map((fl) => [h("span", { class: "tag " + (fl.status === "untracked" ? "" : "warn") }, fl.status), h("span", { class: "mono" }, fl.path)])) : h("div", { class: "muted small" }, "The working tree is clean."));
      }, () => {});
    };
    load();
    let lastHead = t.git && t.git.head + "|" + t.git.remote_head + "|" + t.git.dirty_files;
    ctx.onUpdate = () => { const x = T(id); const k = x && x.git && x.git.head + "|" + x.git.remote_head + "|" + x.git.dirty_files; if (k !== lastHead) { lastHead = k; load(); } };
    return [
      live((x) => {
        const g = x.git;
        if (!g) return h("div", { class: "box muted" }, "Inspecting repository\u2026");
        if (!g.is_repo) return h("div", { class: "note-box" }, g.error || "This directory is not a Git repository, so there is nothing to track. Deployments still work.");
        const canPull = g.behind > 0 && g.ahead === 0;
        return h("div", { class: "box", style: "margin-bottom:22px" },
          h("div", { class: "rowf", style: "margin-bottom:12px" }, gitTag(x),
            h("span", { class: "right rowf" }, btn("Fetch", () => act(() => api("POST", "/api/targets/" + id + "/git/refresh"), "Fetched origin"), "sm", "restart"),
              btn("Pull", () => act(async () => { const r = await api("POST", "/api/targets/" + id + "/git/pull"); toast(r.updated ? "Pulled " + r.commits + " commit(s)" : "Already up to date"); }), "sm" + (canPull ? " hl" : ""), "pull"))),
          facts([
            ["Remote", g.remote_url ? h("span", { class: "mono" }, g.remote_url) : "no origin remote"],
            ["Checked out", h("span", {}, g.checked_out_branch || "detached HEAD", " \u00b7 ", h("span", { class: "mono" }, short(g.head)), " ", g.head_summary || "")],
            ["origin/" + g.tracked_branch, g.remote_head ? h("span", {}, h("span", { class: "mono" }, short(g.remote_head)), " ", g.remote_summary || "") : "branch not found on origin"],
            ["Difference", g.behind + " behind, " + g.ahead + " ahead" + (g.dirty_files ? ", " + g.dirty_files + " changed file(s) in the working tree" : "")],
            ["Last fetch", g.fetch_error ? h("span", { style: "color:var(--err)" }, "failed: " + g.fetch_error) : g.last_fetch ? when(parseUTC(g.last_fetch)) : "pending"],
          ]));
      }),
      saveForm(t, [
        field("Branch to track", branch, "Compared against origin/<branch>. Origin is fetched every 5 minutes; fetching never changes your files."),
        check(auto, "Auto-deploy", "When origin has new commits, fast-forward the working tree and run the script (a running service is restarted). Skipped if tracked files were edited locally or the branch has unpushed commits."),
      ], () => ({ git_branch: branch.value, auto_deploy: auto.checked })),
      h("h3", {}, "Commits"), commits, h("h3", {}, "Working tree changes"), changes,
    ];
};

CFG.health = (ctx) => {
  const { t, id, live } = ctx;
    const url = h("input", { type: "text", value: t.health_url || "", placeholder: "http://127.0.0.1:8000/", spellcheck: "false" });
    const chart = h("div");
    const load = () => api("GET", "/api/targets/" + id + "/health/history").then((d) => {
      const ok = d.samples.filter((x) => x.latency_ms !== null);
      chart.replaceChildren(chartBlock("Response time", ok.length ? ok[ok.length - 1].latency_ms + " ms" : "", ok.map((x) => ({ t: x.t, v: x.latency_ms })),
        { fmt: (v) => Math.round(v) + " ms", note: "last " + d.samples.length + " probes", empty: "No successful probes yet", label: "Health probe response time" }));
    }, () => {});
    if (t.health_url) load();
    ctx.onUpdate = () => { if (T(id) && T(id).health_url) load(); };
    return [
      live((x) => !x.health_url ? h("div", { class: "note-box" }, "No health URL is configured, so this target has no LIVE / SLOW / DOWN status.")
        : h("div", { class: "box", style: "margin-bottom:22px" },
          h("div", { class: "rowf", style: "margin-bottom:12px" }, x.health ? h("span", { class: "pill " + ({ LIVE: "ok", SLOW: "warn", DOWN: "err" }[x.health.status]) }, x.health.status) : h("span", { class: "pill off" }, "Checking"),
            strip(x, 40), btn("Check now", () => act(() => api("POST", "/api/targets/" + id + "/health/check")), "sm right")),
          facts([
            ["Last result", x.health ? [x.health.http_status ? "HTTP " + x.health.http_status : null, x.health.latency_ms !== null ? x.health.latency_ms + " ms" : null, x.health.detail].filter(Boolean).join(" \u00b7 ") || "-" : "-"],
            ["Checked", x.health ? when(parseUTC(x.health.checked_at)) : "-"],
            ["Reachable", x.uptime_pct !== null ? x.uptime_pct + "% of the last " + ((x.health_recent || []).length >= 40 ? "hour" : "probes") : "-"],
          ]))),
      t.health_url ? chart : null,
      saveForm(t, [field("Health URL", url, "LIVE: answered in under 1 s. SLOW: 1 s or more. DOWN: no connection, timeout after 5 s, or HTTP 5xx. For https:// only TCP reachability is checked. Leave empty to disable.")],
        () => ({ health_url: url.value })),
    ];
};

CFG.schedule = (ctx) => {
  const { t, id, live } = ctx;
    const cron = h("input", { type: "text", value: t.schedule || "", placeholder: "0 2 * * *", spellcheck: "false", class: "mono" });
    const prev = h("div", { class: "hint" });
    let timer = null;
    const preview = () => {
      const v = cron.value.trim();
      if (!v) return prev.replaceChildren("No schedule: this target only runs when started manually, by autostart, auto-deploy or webhook.");
      api("GET", "/api/cron?expr=" + encodeURIComponent(v)).then((r) => {
        if (cron.value.trim() !== v) return;
        prev.replaceChildren(r.valid ? ["Next runs (local time): ", r.next.map((x) => fmtDT(new Date(x * 1000)).slice(0, 16)).join("  \u00b7  ")] : h("span", { class: "errtxt" }, r.error));
      }, () => {});
    };
    cron.oninput = () => { clearTimeout(timer); timer = setTimeout(preview, 250); };
    preview();
    const presets = [["Every 5 minutes", "*/5 * * * *"], ["Hourly", "0 * * * *"], ["Daily at 02:00", "0 2 * * *"], ["Weekdays at 07:30", "30 7 * * 1-5"], ["Sundays at 03:00", "0 3 * * 0"], ["1st of the month", "0 0 1 * *"]];
    return [
      h("div", { class: "note-box" }, "Runs the script on a cron schedule in this machine's local time, replacing a Task Scheduler or launchd entry. ",
        t.run_mode === "SERVICE" ? "Because this target is a service, a scheduled run restarts it." : "If the previous run is still in progress, the scheduled run is skipped."),
      saveForm(t, [
        field("Cron expression", cron, "minute  hour  day-of-month  month  day-of-week. Supports lists (1,15), ranges (1-5) and steps (*/10)."),
        h("div", { class: "rowf", style: "margin:-6px 0 10px" }, presets.map(([label, v]) => btn(label, () => { cron.value = v; preview(); }, "sm")), btn("Clear", () => { cron.value = ""; preview(); }, "sm ghost")),
        h("div", { style: "margin-bottom:14px" }, prev),
      ], () => ({ schedule: cron.value }), "Schedule saved"),
    ];
};

CFG.webhooks = (ctx) => {
  const { t, id, live } = ctx;
    return [live((x) => {
      const url = x.webhook_token ? location.origin + "/api/webhooks/deploy/" + x.webhook_token : null;
      return [
        h("div", { class: "note-box" }, "A deploy webhook lets a Git server, a CI job or a script trigger this target: it pulls from origin (fast-forward only) and runs the script. The address only works from this machine or your private network, and the token in it is the credential, so treat it like a password."),
        url ? h("div", { class: "box" },
          field("Deploy webhook URL", h("input", { type: "text", readonly: true, value: url, class: "mono", onfocus: (e) => e.target.select() })),
          h("div", { class: "rowf", style: "margin-bottom:14px" }, btn("Copy URL", () => copyText(url), "sm", "copy"),
            btn("Regenerate", async () => { if (await confirmDlg("Regenerate webhook URL?", "The current URL stops working immediately.", "Regenerate")) act(() => api("POST", "/api/targets/" + id + "/webhook"), "New webhook URL generated"); }, "sm"),
            btn("Disable", () => act(() => api("DELETE", "/api/targets/" + id + "/webhook"), "Webhook disabled"), "sm danger")),
          h("div", { class: "hint" }, "Example:"), h("pre", { class: "mono", style: "white-space:pre-wrap;word-break:break-all;margin:4px 0 0;color:var(--muted)" }, "curl -X POST " + url),
          h("div", { class: "hint", style: "margin-top:8px" }, "Add ?pull=0 to run the script without pulling. Responds 202 with the run id, or 409 if a task is already running or the pull is not a clean fast-forward. A push payload for another branch or a tag is acknowledged without deploying."))
          : h("div", { class: "box rowf" }, h("span", { class: "muted" }, "No webhook URL exists for this target."), btn("Generate webhook URL", () => act(() => api("POST", "/api/targets/" + id + "/webhook"), "Webhook URL generated"), "hl right")),
        url ? h("div", { class: "box", style: "margin-top:14px" },
          h("h3", { style: "margin-top:0" }, "Signing secret"),
          x.webhook_secret
            ? [h("p", { class: "muted small", style: "margin-top:0" }, "Every call must now prove this secret, or it is refused with 401. GitHub, Gitea and Forgejo: paste it as the webhook Secret (content type application/json). GitLab: paste it as the Secret token. A script: send it in an X-Webhook-Secret header."),
              field("Secret", h("input", { type: "text", readonly: true, value: x.webhook_secret, class: "mono", onfocus: (e) => e.target.select() })),
              h("div", { class: "rowf", style: "margin-bottom:10px" }, btn("Copy secret", () => copyText(x.webhook_secret), "sm", "copy"),
                btn("Replace", async () => { if (await confirmDlg("Replace the signing secret?", "Senders using the current secret are refused until they are given the new one.", "Replace")) act(() => api("POST", "/api/targets/" + id + "/webhook/secret"), "New signing secret set"); }, "sm"),
                btn("Remove", async () => { if (await confirmDlg("Remove the signing secret?", "The URL alone will be enough to trigger a deploy again.", "Remove", true)) act(() => api("DELETE", "/api/targets/" + id + "/webhook/secret"), "Signing secret removed"); }, "sm danger")),
              h("div", { class: "hint" }, "Example:"), h("pre", { class: "mono", style: "white-space:pre-wrap;word-break:break-all;margin:4px 0 0;color:var(--muted)" }, "curl -X POST -H \"X-Webhook-Secret: " + x.webhook_secret + "\" " + url)]
            : h("div", { class: "rowf" }, h("span", { class: "muted small", style: "flex:1" }, "Optional. With a secret, a leaked URL is not enough to deploy: callers must also sign the request (GitHub, Gitea) or send the secret (GitLab, scripts)."),
                btn("Add signing secret", () => act(() => api("POST", "/api/targets/" + id + "/webhook/secret"), "Signing secret set"), "sm hl")))
          : null,
      ];
    })];
};

CFG.backups = (ctx) => {
  const { t, id, live } = ctx;
    const file = h("input", { type: "text", value: t.db_file || "", placeholder: "db.sqlite3", spellcheck: "false", list: "dblist" });
    const auto = h("input", { type: "checkbox", checked: t.backup_on_deploy });
    const dblist = h("datalist", { id: "dblist" });
    api("GET", "/api/fs/inspect?dir=" + encodeURIComponent(t.working_directory)).then((r) => {
      dblist.replaceChildren((r.sqlite_files || []).map((f) => h("option", { value: f })));
      if (!file.value && (r.sqlite_files || []).length === 1) file.placeholder = r.sqlite_files[0] + "  (found in the folder)";
    }, () => {});
    const listBox = h("div");
    let info = null;
    const paintList = () => {
      const x = T(id); if (!x) return;
      if (!x.db_file) return listBox.replaceChildren(h("div", { class: "muted small" }, "Set the database file above to enable backups."));
      if (!info) return listBox.replaceChildren(h("p", { class: "muted" }, "Loading…"));
      const busy = !!(x.running || x.command_running);
      listBox.replaceChildren(
        h("div", { class: "rowf", style: "margin-bottom:10px" }, h("span", { class: "muted small", style: "flex:1" }, info.backups.length + " snapshot(s), the newest " + info.keep + " are kept. Stored in ", h("span", { class: "mono" }, info.directory)),
          btn("Back up now", async () => { try { const b = await api("POST", "/api/targets/" + id + "/backups"); toast("Backup created: " + b.file); load(); } catch (e) { toast(e.message, true); } }, "hl sm", "download")),
        info.backups.length ? table(["Snapshot", "Size", "Taken", ""], info.backups.map((b) => [h("span", { class: "mono" }, b.file), fmtBytes(b.size), [fmtDT(new Date(b.created * 1000)), h("span", { class: "dim" }, "  (" + fmtAgo(new Date(b.created * 1000)) + ")")],
          h("div", { class: "rowf", style: "justify-content:flex-end;flex-wrap:nowrap" },
            btn("Restore", async () => {
              if (!(await confirmDlg("Restore this snapshot?", [h("p", {}, "The database ", h("span", { class: "mono" }, x.db_file), " is replaced with the snapshot from " + fmtDT(new Date(b.created * 1000)) + "."),
                h("p", {}, "Everything written since then is removed from the live database. The current state is snapshotted first, so the restore can be undone.")], "Restore", true))) return;
              try { const r = await api("POST", "/api/targets/" + id + "/backups/" + encodeURIComponent(b.file) + "/restore"); toast("Restored. Previous state saved as " + r.safety_backup.file); load(); } catch (e) { toast(e.message, true); }
            }, "sm", "restart", { disabled: busy, title: busy ? "Stop the target first" : "" }),
            btn("Delete", async () => { if (!(await confirmDlg("Delete snapshot?", b.file, "Delete", true))) return; try { await api("DELETE", "/api/targets/" + id + "/backups/" + encodeURIComponent(b.file)); load(); } catch (e) { toast(e.message, true); } }, "sm danger", "trash"))]))
          : h("div", { class: "muted small" }, "No snapshots yet."),
        busy && info.backups.length ? h("div", { class: "hint", style: "margin-top:8px" }, "Restore is disabled while the target or a command is running.") : null);
    };
    const load = () => api("GET", "/api/targets/" + id + "/backups").then((d) => { info = d; paintList(); }, () => {});
    load();
    ctx.onUpdate = paintList;
    return [
      h("div", { class: "note-box" }, "Consistent snapshots of the project's SQLite database, taken through a read-only connection, so they are safe while the application is running and never modify the database. Snapshots are stored in PortalControl's data folder, outside the project."),
      saveForm(t, [
        field("SQLite database file", [file, dblist], "Path inside the working directory, e.g. db.sqlite3. Leave empty to disable backups."),
        check(auto, "Back up before every run", "Take a snapshot before the script starts (manual, scheduled, auto-deploy and webhook runs) and before the “Apply migrations” command. Automatic crash restarts are skipped."),
      ], () => ({ db_file: file.value, backup_on_deploy: auto.checked }), "Backup settings saved"),
      h("h3", {}, "Snapshots"), listBox,
    ];
};

CFG.advanced = (ctx) => {
  const { t, id, live } = ctx;
    const autostart = h("input", { type: "checkbox", checked: t.autostart });
    const restart = h("input", { type: "checkbox", checked: t.auto_restart });
    const timeout = h("input", { type: "number", min: "1", max: "86400", value: t.timeout_secs });
  const order = h("input", { type: "number", min: "0", max: "999", value: t.start_order });
    return [saveForm(t, [
      check(autostart, "Start with PortalControl", "Launch this target whenever the manager starts. Combined with \u201cStart at login\u201d in Settings, it comes up with the machine."),
      check(restart, "Restart on crash", "Services only. If the process exits on its own it is started again after 5 seconds. After 5 crashes in 10 minutes automatic restart is suspended until you start it manually."),
      field("Task timeout (seconds)", timeout, "A task still running after this long is terminated together with its whole process tree. Services have no timeout."),
      field("Start order in the project", order, "Used by “Start all” on the project page: lower numbers start first, and each step must be ready before the next begins. A task has to finish successfully; a service has to stay up and, if it has a health URL, answer it. Example: migrations 0, web server 1, queue cluster 2."),
    ], () => ({ autostart: autostart.checked, auto_restart: restart.checked, timeout_secs: parseInt(timeout.value, 10) || 300, start_order: parseInt(order.value, 10) || 0 }))];
};

CFG.danger = (ctx) => {
  const { t, id, live } = ctx;
    return [h("div", { class: "dangerzone" },
      h("div", {}, h("div", { class: "grow" }, h("b", {}, "Clear deployment history"), h("div", { class: "muted small" }, "Deletes the stored runs and their captured logs. A run in progress is kept.")),
        btn("Clear history", async () => { if (await confirmDlg("Clear deployment history?", "All finished runs of \u201c" + t.name + "\u201d and their logs are deleted. This cannot be undone.", "Clear history", true)) act(() => api("DELETE", "/api/targets/" + id + "/history"), "History cleared"); }, "danger")),
      h("div", {}, h("div", { class: "grow" }, h("b", {}, "Delete this target"), h("div", { class: "muted small" }, "Stops it if running and removes it from PortalControl with its history and scan reports. The project folder is not touched.")),
        btn("Delete target", async () => {
          if (!(await confirmDlg("Delete \u201c" + t.name + "\u201d?", "The target, its deployment history and its security reports are removed from PortalControl. Files in " + t.working_directory + " are not touched.", "Delete target", true))) return;
          try { await api("DELETE", "/api/targets/" + id); toast("Target deleted"); await refresh(); go("projects"); } catch (e) { toast(e.message, true); }
        }, "danger", "trash")))];
};

function cfgBody(id, sub) {
  const t = T(id);
  const lives = [];
  const live = (paint) => { const c = h("div"); const run = () => { const x = T(id); if (x) c.replaceChildren(...[paint(x)].flat().filter(Boolean)); }; lives.push(run); run(); return c; };
  const ctx = { t, id, live, onUpdate: null };
  const content = CFG[sub] ? CFG[sub](ctx) : [empty("Unknown section", "")];
  const el = h("div", { class: "cfg" },
    h("nav", { class: "subnav" }, SUBS.map(([k, label]) => h("a", { href: "#/target/" + id + "/configuration/" + k, class: (k === sub ? "on" : "") + (k === "danger" ? " dz" : "") }, label,
      k === "environment" && t.env_count ? h("span", { class: "dim small" }, "  " + t.env_count) : null))),
    h("div", { style: "min-width:0" }, h("h2", { style: "margin-top:0" }, (SUBS.find((x) => x[0] === sub) || ["", ""])[1]), content));
  return { el, update: () => { lives.forEach((f) => f()); if (ctx.onUpdate) ctx.onUpdate(); } };
}
