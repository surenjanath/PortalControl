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
