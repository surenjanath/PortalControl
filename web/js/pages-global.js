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
function securityView() {
  return simple("security", () => {
    const scanned = targets.filter((t) => t.scan);
    return [
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Security"), h("p", { class: "sub" }, "Static analysis and dependency audit results for every target.")),
        btn("Scan all targets", () => targets.filter((t) => !t.scan_running).forEach(doScan), "hl", "shield", { disabled: !targets.length })),
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
// Settings → Access: set, change or remove the dashboard password.
function accessSection() {
  const set = !!(authInfo && authInfo.password_set);
  const min = (authInfo && authInfo.min_password_chars) || 8;
  const current = h("input", { type: "password", autocomplete: "current-password" });
  const fresh = h("input", { type: "password", autocomplete: "new-password", minlength: String(min) });
  const again = h("input", { type: "password", autocomplete: "new-password" });
  const err = h("span", { class: "errtxt" });
  const send = async (body, done) => {
    err.textContent = "";
    try { await api("POST", "/api/auth/password", body); toast(done); await refresh(true); }
    catch (e) { err.textContent = e.message; }
  };
  const form = h("form", { style: "max-width:520px;margin-top:14px", onsubmit: (e) => {
      e.preventDefault();
      if (fresh.value !== again.value) { err.textContent = "The two new passwords are not the same"; return; }
      if (fresh.value.length < min) { err.textContent = "Use at least " + min + " characters"; return; }
      send({ current: current.value, new: fresh.value }, set ? "Password changed. Other browsers were signed out." : "Password set. This browser stays signed in.");
    } },
    set ? field("Current password", current) : null,
    h("div", { class: "cols" }, field(set ? "New password" : "Password", fresh, "At least " + min + " characters."), field("Repeat it", again)),
    h("div", { class: "formfoot" }, h("button", { class: "btn hl", type: "submit" }, set ? "Change password" : "Set password"),
      set ? btn("Remove password", async () => {
        if (!current.value) { err.textContent = "Enter the current password to remove it"; return; }
        if (await confirmDlg("Remove the password?", "Anyone who can reach this address will be able to use the dashboard again.", "Remove password", true)) send({ current: current.value, new: "" }, "Password removed");
      }, "danger") : null, err));
  return [h("h2", { style: "margin-top:0" }, "Access"),
    h("div", { class: "box", style: "margin-bottom:26px" },
      set ? h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "Password protected") : h("span", { class: "pill warn" }, h("span", { class: "dot warn" }), "No password"),
      h("div", { class: "muted small", style: "margin-top:8px" }, set
        ? "Sign-in is required for the dashboard and its API. Sessions last 7 days and end when the manager restarts. Deploy webhooks keep working through their own token. Forgotten password: on this machine run portalcontrol stop, then portalcontrol reset-password."
        : "Anyone who can reach " + (sys ? "http://" + sys.bind : "this address") + " can run scripts, commands and database restores. That is fine on 127.0.0.1 for a single-user machine; set a password before binding to a LAN or Tailscale address."),
      form)];
}

function settingsView() {
  const def = { key: "settings" };
  def.build = () => {
    const info = h("div"), charts = h("div", { class: "grid g2" }), logPre = h("pre", { class: "log", style: "height:300px" }), logPath = h("span", { class: "dim small mono" });
    const paintInfo = () => {
      if (!sys) return;
      const host = sys.host || {};
      info.replaceChildren(
        h("h2", { style: "margin-top:0" }, "Start at login"),
        h("div", { class: "box rowf" }, h("div", { style: "flex:1;min-width:240px" }, sys.autostart ? [h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "Enabled"), h("div", { class: "mono dim", style: "margin-top:8px;word-break:break-all" }, sys.autostart)]
            : [h("span", { class: "pill off" }, "Disabled"), h("div", { class: "muted small", style: "margin-top:8px" }, "PortalControl only runs while you start it yourself. Enable this to bring it (and every target marked \u201cStart with PortalControl\u201d) up when you log in.")]),
          sys.autostart ? btn("Disable", () => act(() => api("DELETE", "/api/system/autostart"), "Start at login disabled"), "danger") : btn("Enable start at login", () => act(() => api("POST", "/api/system/autostart"), "Start at login enabled"), "hl")),
        h("h2", {}, "Notifications"),
        h("div", { class: "box rowf" }, h("div", { style: "flex:1;min-width:240px" }, notifyOn() ? h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "On in this browser") : h("span", { class: "pill off" }, "Off"),
            h("div", { class: "muted small", style: "margin-top:8px" }, "Desktop notifications when a run fails, a service goes down or crashes, and when a run finishes while this tab is in the background. Needs the dashboard open in a browser tab.")),
          btn(notifyOn() ? "Turn off" : "Turn on notifications", toggleNotify, notifyOn() ? "" : "hl")),
        h("h2", {}, "Export and import"),
        h("div", { class: "box rowf" }, h("div", { class: "muted small", style: "flex:1;min-width:240px" }, "Save every target's configuration (including environment variables, but not history, snapshots or webhook tokens) to a JSON file, or load one on another machine. On import, targets that are already registered are skipped and ones whose folder does not exist here are reported."),
          btn("Export", async () => {
            try {
              const data = await api("GET", "/api/export");
              const a = h("a", { href: URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: "application/json" })), download: "portalcontrol-targets.json" });
              document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(a.href), 2000);
            } catch (e) { toast(e.message, true); }
          }, "", "download"),
          btn("Import…", () => {
            const pick = h("input", { type: "file", accept: ".json,application/json", style: "display:none" });
            pick.onchange = async () => {
              try {
                const parsed = JSON.parse(await pick.files[0].text());
                if (!Array.isArray(parsed.targets)) throw new Error("This file is not a PortalControl export");
                const r = await api("POST", "/api/import", { targets: parsed.targets });
                await confirmDlg("Import finished", [h("p", {}, r.created + " created, " + r.skipped + " already registered" + (r.errors.length ? ", " + r.errors.length + " could not be imported:" : ".")),
                  r.errors.map((x) => h("p", { class: "small" }, x))], "OK");
                refresh(true);
              } catch (e) { toast(e.message, true); }
              pick.remove();
            };
            document.body.append(pick); pick.click();
          })),
        h("h2", {}, "Manager"),
        h("div", { class: "box" }, facts([
          ["Version", "PortalControl-Core " + sys.version], ["Listening on", h("span", { class: "mono" }, "http://" + sys.bind)], ["Process", "pid " + sys.pid + " \u00b7 up " + fmtUp(sys.uptime_secs) + " \u00b7 " + sys.worker_threads + " worker threads"],
          ["Executable", h("span", { class: "mono" }, sys.executable)], ["Data directory", h("span", { class: "mono" }, sys.data_dir)],
          ["Host", [host.name, host.os_version, sys.arch, host.cpus ? host.cpus + " cores" : null, host.mem_total ? fmtBytes(host.mem_total) + " RAM" : null].filter(Boolean).join(" \u00b7 ")],
          ["Shell", h("span", { class: "mono" }, sys.shell)], ["Connections", sys.ws_clients + " dashboard(s) \u00b7 " + sys.active_runs + " running target(s) \u00b7 " + sys.active_scans + " scan(s)"],
          ["Intervals", "health every " + sys.intervals.health_probe_secs + " s \u00b7 working tree every " + sys.intervals.git_inspect_secs + " s \u00b7 fetch every " + sys.intervals.git_fetch_secs / 60 + " min \u00b7 metrics every " + sys.intervals.metrics_secs + " s"],
          ["Network", "Loopback, private-LAN and Tailscale addresses only. There is no login: anyone who can reach this address can run the registered scripts."]])));
    };
    const load = () => {
      api("GET", "/api/system/metrics").then((m) => {
        if (charts.querySelector(".chart:hover")) return;
        const last = m.samples[m.samples.length - 1];
        charts.replaceChildren(
          h("div", { class: "box" }, chartBlock("Host CPU", last ? last.cpu.toFixed(0) + " %" : "", m.samples.map((x) => ({ t: x.t, v: x.cpu })), { max: 100, fmt: (v) => Math.round(v) + "%", note: m.cpus + " cores", label: "Host CPU usage" })),
          h("div", { class: "box" }, chartBlock("Host memory", last ? fmtBytes(last.mem_used) : "", m.samples.map((x) => ({ t: x.t, v: x.mem_used })), { max: m.mem_total, fmt: fmtBytes, note: "of " + fmtBytes(m.mem_total), label: "Host memory in use" })),
          h("div", { class: "box" }, chartBlock("PortalControl memory", last ? fmtBytes(last.manager_mem) : "", m.samples.map((x) => ({ t: x.t, v: x.manager_mem })), { fmt: fmtBytes, note: "the manager process itself", label: "Manager memory" })),
          h("div", { class: "box" }, chartBlock("PortalControl CPU", last ? last.manager_cpu.toFixed(2) + " %" : "", m.samples.map((x) => ({ t: x.t, v: x.manager_cpu })), { minTop: 1, fmt: (v) => v.toFixed(1) + "%", note: "of all cores", label: "Manager CPU" })));
      }, () => {});
      api("GET", "/api/system/log").then((d) => {
        logPath.textContent = d.path || "";
        const atEnd = logPre.scrollTop + logPre.clientHeight >= logPre.scrollHeight - 30;
        logPre.textContent = d.log || "(empty)";
        if (atEnd) logPre.scrollTop = logPre.scrollHeight;
      }, () => {});
    };
    paintInfo(); load();
    const timer = setInterval(load, 5000);
    // The Access form is built once so typing in it survives background refreshes;
    // the page is rebuilt only when the password state itself changes.
    const builtWithPassword = !!(authInfo && authInfo.password_set);
    def.update = () => { if (!!(authInfo && authInfo.password_set) !== builtWithPassword) return show(true); paintInfo(); };
    def.cleanup = () => clearInterval(timer);
    return h("div", {}, h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Settings"), h("p", { class: "sub" }, "The manager itself: startup, resources and its own log."))),
      accessSection(), info, h("h2", {}, "Resources", h("span", { class: "dim small", style: "font-weight:400" }, "  last 15 minutes")), charts,
      h("h2", {}, "Manager log  ", logPath), h("div", { class: "logbox" }, logPre),
      h("h2", {}, "Danger Zone"),
      h("div", { class: "dangerzone" }, h("div", {}, h("div", { class: "grow" }, h("b", {}, "Shut down PortalControl"), h("div", { class: "muted small" }, "Stops the manager and every process it is supervising. This page stops responding until it is started again.")),
        btn("Shut down", async () => { if (await confirmDlg("Shut down PortalControl?", "All running targets are stopped. You will have to start PortalControl again from a terminal or by logging in.", "Shut down", true)) act(() => api("POST", "/api/system/shutdown"), "Shutting down\u2026"); }, "danger"))));
  };
  return def;
}

