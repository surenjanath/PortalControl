"use strict";
// ---- settings: the manager itself, its resource use, its log, shutting it down ----
SET.manager = () => {
  const c = h("div");
  const paint = () => {
    if (!sys) return;
    const host = sys.host || {};
    c.replaceChildren(
        h("div", { class: "box" }, facts([
          ["Version", "PortalControl-Core " + sys.version], ["Listening on", h("span", { class: "mono" }, "http://" + sys.bind)], ["Process", "pid " + sys.pid + " \u00b7 up " + fmtUp(sys.uptime_secs) + " \u00b7 " + sys.worker_threads + " worker threads"],
          ["Executable", h("span", { class: "mono" }, sys.executable)], ["Data directory", h("span", { class: "mono" }, sys.data_dir)],
          ["Host", [host.name, host.os_version, sys.arch, host.cpus ? host.cpus + " cores" : null, host.mem_total ? fmtBytes(host.mem_total) + " RAM" : null].filter(Boolean).join(" \u00b7 ")],
          ["Disks", (host.disks || []).length ? h("span", {}, host.disks.map((d) => h("div", {}, h("span", { class: "mono" }, d.mount), " " + fmtBytes(d.free) + " free of " + fmtBytes(d.total) + " (" + Math.round(((d.total - d.free) / d.total) * 100) + "% used)"))) : "-"],
          ["Shell", h("span", { class: "mono" }, sys.shell)], ["Connections", sys.ws_clients + " dashboard(s) \u00b7 " + sys.active_runs + " running target(s) \u00b7 " + sys.active_scans + " scan(s)"],
          ["Intervals", "health every " + sys.intervals.health_probe_secs + " s \u00b7 working tree every " + sys.intervals.git_inspect_secs + " s \u00b7 fetch every " + sys.intervals.git_fetch_secs / 60 + " min \u00b7 metrics every " + sys.intervals.metrics_secs + " s"],
          ["Network", "See Network above. Only this computer, private-LAN and Tailscale addresses can ever be used."]]))
    );
  };
  paint();
  return { el: c, update: paint };
};

SET.resources = () => {
  const charts = h("div", { class: "grid g2" });
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
  };
  load();
  const timer = setInterval(load, 5000);
  return { el: [h("p", { class: "muted small", style: "margin-top:-6px" }, "The last 15 minutes, refreshed every 5 seconds."), charts], cleanup: () => clearInterval(timer) };
};

// Scanner tools (bandit, pip-audit) kept in PortalControl's own environment; see toolbox.rs.
SET.tools = () => {
  const c = h("div", { class: "muted" }, "Checking\u2026");
  let timer = null, busy = false;
  const label = { managed: ["ok", "Installed by PortalControl"], system: ["info", "Found on this computer"], broken: ["err", "Broken \u2014 repair it"], missing: ["warn", "Not installed"] };
  const load = async () => {
    if (busy) return;
    busy = true;
    try {
      const s = await api("GET", "/api/system/tools");
      const allOk = s.tools.every((t) => t.source === "managed" || t.source === "system");
      const install = (mode) => act(() => api("POST", "/api/system/tools/install", { mode }), { install: "Installing", repair: "Reinstalling", upgrade: "Updating" }[mode] + " scanner tools\u2026").then(() => setTimeout(load, 500));
      c.className = "";
      c.replaceChildren(
        h("div", { class: "note-box" }, "Security scans use bandit and pip-audit. If one is missing or broken when a scan starts, PortalControl installs it by itself into its own environment in the data folder and runs the scan again. Your system Python and your projects are never changed. Installing needs Python 3 and an internet connection to PyPI."),
        h("div", { class: "box" },
          h("table", { class: "tbl" }, h("thead", {}, h("tr", {}, h("th", {}, "Tool"), h("th", {}, "State"), h("th", {}, "Version"))),
            h("tbody", {}, s.tools.map((t) => { const [cls, text] = label[t.source] || ["", t.source]; return h("tr", {}, h("td", { class: "mono" }, t.name), h("td", {}, h("span", { class: "pill " + cls }, h("span", { class: "dot " + cls }), text)), h("td", { class: "mono small" }, t.version || "\u2013")); }))),
          facts([["Python for installing", s.python ? h("span", { class: "mono" }, s.python.command + " (" + s.python.version + ")") : h("span", { class: "t-err" }, "No Python 3 found \u2014 install it from python.org first")],
            ["Tool environment", h("span", { class: "mono small" }, s.dir || "\u2013")],
            ["Last install", s.installing ? h("span", {}, h("span", { class: "dot info pulse" }), " Installing now\u2026") : s.last && s.last.at ? h("span", { class: s.last.ok ? "" : "t-err" }, s.last.at.replace("T", " ").replace("Z", " UTC") + " \u00b7 " + s.last.message) : "Never"]]),
          h("div", { class: "rowf", style: "margin-top:12px" },
            btn(allOk ? "Reinstall" : "Install now", () => install(allOk || s.tools.some((t) => t.source === "broken") ? "repair" : "install"), allOk ? "" : "hl", "download", { disabled: s.installing || !s.python }),
            btn("Update to latest", () => install("upgrade"), "", "restart", { disabled: s.installing || !s.python }))));
      clearTimeout(timer);
      if (s.installing) timer = setTimeout(load, 2500);
    } catch (e) { c.replaceChildren(h("div", { class: "note-box err" }, e.message)); }
    busy = false;
  };
  load();
  return { el: c, update: () => {}, cleanup: () => clearTimeout(timer) };
};

SET.log = () => {
  const logPre = h("pre", { class: "log", style: "height:460px" }), logPath = h("span", { class: "dim small mono" });
  const load = () => {
      api("GET", "/api/system/log").then((d) => {
        logPath.textContent = d.path || "";
        const atEnd = logPre.scrollTop + logPre.clientHeight >= logPre.scrollHeight - 30;
        logPre.textContent = d.log || "(empty)";
        if (atEnd) logPre.scrollTop = logPre.scrollHeight;
      }, () => {});
  };
  load();
  const timer = setInterval(load, 5000);
  return { el: [h("div", { style: "margin:-6px 0 10px" }, logPath), h("div", { class: "logbox" }, logPre)], cleanup: () => clearInterval(timer) };
};

SET.danger = () =>
  h("div", { class: "dangerzone" }, h("div", {}, h("div", { class: "grow" }, h("b", {}, "Shut down PortalControl"), h("div", { class: "muted small" }, "Stops the manager and every process it is supervising. This page stops responding until it is started again.")),
        btn("Shut down", async () => { if (await confirmDlg("Shut down PortalControl?", "All running targets are stopped. You will have to start PortalControl again from a terminal or by logging in.", "Shut down", true)) act(() => api("POST", "/api/system/shutdown"), "Shutting down\u2026"); }, "danger")));
