"use strict";
// ----------------------------------------------------------- notifications --
// Desktop notifications for problems (and for finished runs while the tab is
// in the background). Opt-in, remembered per browser.
const notifyOn = () => { try { return localStorage.getItem("pc-notify") === "1" && "Notification" in window && Notification.permission === "granted"; } catch (_) { return false; } };
function notify(e, t) {
  if (!notifyOn()) return;
  const problem = e.level === "error" || (e.level === "warn" && (e.kind === "health" || e.kind === "restart"));
  const finished = e.kind === "run" && e.level === "ok" && document.hidden;
  if (!problem && !finished) return;
  try {
    const n = new Notification((t ? t.name : "PortalControl") + (problem ? " needs attention" : ""), { body: e.message, tag: "pc-" + (e.target_id || 0) + "-" + e.kind });
    n.onclick = () => { window.focus(); if (t) go("target/" + t.id); n.close(); };
  } catch (_) {}
}
async function toggleNotify() {
  if (!("Notification" in window)) return toast("This browser does not support notifications", true);
  if (notifyOn()) { localStorage.setItem("pc-notify", "0"); toast("Desktop notifications off"); return show(true); }
  const p = Notification.permission === "granted" ? "granted" : await Notification.requestPermission();
  if (p !== "granted") return toast("Notifications are blocked for this site in the browser settings", true);
  localStorage.setItem("pc-notify", "1");
  toast("Desktop notifications on");
  show(true);
}

// ---------------------------------------------------------- command palette --
// Ctrl+K (or /): jump to any page, project or target, or run a target action.
function openPalette() {
  if ($("palette")) return;
  const items = [];
  [["Dashboard", ""], ["Projects", "projects"], ["New target", "new"], ["Deployments", "deployments"], ["Security", "security"], ["Incidents", "incidents"], ["Analytics", "analytics"], ["Activity", "activity"], ["Settings", "settings"]]
    .forEach(([label, path]) => items.push({ label, hint: "Page", run: () => go(path) }));
  byProject(targets).forEach(([name]) => items.push({ label: name, hint: "Project terminals", run: () => { location.hash = projHref(name); } }));
  targets.forEach((t) => {
    items.push({ label: t.name, hint: tStatus(t).text, run: () => go("target/" + t.id) });
    items.push({ label: t.name + ": logs", hint: "Open", run: () => go("target/" + t.id + "/logs") });
    items.push({ label: t.name + ": commands", hint: "Open", run: () => go("target/" + t.id + "/commands") });
    if (t.running) {
      items.push({ label: t.name + ": stop", hint: "Action", run: () => doStop(t) });
      if (t.run_mode === "SERVICE") items.push({ label: t.name + ": restart", hint: "Action", run: () => doRestart(t) });
    } else items.push({ label: t.name + (t.run_mode === "SERVICE" ? ": start" : ": deploy"), hint: "Action", run: () => doRun(t) });
  });
  let shown = [], sel = 0;
  const list = h("div", { style: "max-height:46vh;overflow:auto;padding:6px" });
  const input = h("input", { type: "search", placeholder: "Jump to a page, project or target, or type an action…", "aria-label": "Command palette", style: "border:0;border-bottom:1px solid var(--line);border-radius:0;padding:14px 16px;background:transparent;box-shadow:none" });
  const dlg = h("dialog", { id: "palette", style: "width:min(620px,94vw);margin-top:12vh" }, input, list);
  const close = () => { dlg.close(); dlg.remove(); };
  const paint = () => {
    const words = input.value.toLowerCase().split(/\s+/).filter(Boolean);
    shown = items.filter((i) => words.every((w) => (i.label + " " + i.hint).toLowerCase().includes(w))).slice(0, 40);
    sel = Math.min(sel, Math.max(0, shown.length - 1));
    list.replaceChildren(shown.length ? shown.map((i, n) => h("div", { class: "side-t" + (n === sel ? " on" : ""), style: "cursor:pointer;padding:8px 12px", onclick: () => { close(); i.run(); } },
      h("span", { class: "nm", style: "flex:1;color:var(--text)" }, i.label), h("span", { class: "dim small" }, i.hint))) : h("div", { class: "muted", style: "padding:14px" }, "No matches"));
    const on = list.querySelector(".on"); if (on) on.scrollIntoView({ block: "nearest" });
  };
  input.oninput = () => { sel = 0; paint(); };
  input.onkeydown = (e) => {
    if (e.key === "ArrowDown") { sel = Math.min(sel + 1, shown.length - 1); paint(); e.preventDefault(); }
    else if (e.key === "ArrowUp") { sel = Math.max(sel - 1, 0); paint(); e.preventDefault(); }
    else if (e.key === "Enter" && shown[sel]) { const i = shown[sel]; close(); i.run(); e.preventDefault(); }
  };
  dlg.addEventListener("cancel", (e) => { e.preventDefault(); close(); });
  dlg.addEventListener("click", (e) => { if (e.target === dlg) close(); });
  document.body.append(dlg);
  paint();
  dlg.showModal();
}
window.addEventListener("keydown", (e) => {
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test((document.activeElement || {}).tagName || "");
  if ((e.key.toLowerCase() === "k" && (e.ctrlKey || e.metaKey)) || (e.key === "/" && !typing && !document.querySelector("dialog[open]"))) { e.preventDefault(); openPalette(); }
});

// ------------------------------------------------------------ data plumbing --
async function refresh(force) {
  try {
    [targets, sys, authInfo] = await Promise.all([api("GET", "/api/targets"), api("GET", "/api/system"), api("GET", "/api/auth/status")]);
    loaded = true;
    show(force === true);
  } catch (e) { /* server restarting; the poll will retry */ }
}
function scheduleRefresh(ms) {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, ms === undefined ? 300 : ms);
}
function connect() {
  const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws");
  ws.onopen = () => { wsUp = true; api("GET", "/api/activity").then((a) => { activity = a; }, () => {}).then(refresh); };
  ws.onclose = () => { wsUp = false; renderSide(routeParts()); setTimeout(connect, 2000); };
  ws.onmessage = (ev) => {
    let m; try { m = JSON.parse(ev.data); } catch (_) { return; }
    sinks.forEach((fn) => fn(m));
    if (m.type === "log" || m.type === "hello" || m.type === "lagged") return;
    if (m.type === "activity") {
      activity.unshift(m); if (activity.length > 300) activity.pop();
      const t = m.target_id ? T(m.target_id) : null;
      if (m.level === "error") toast((t ? t.name + ": " : "") + m.message, true);
      notify(m, t);
    }
    scheduleRefresh();
  };
}
window.addEventListener("hashchange", () => show());
show();
connect();
refresh();
setInterval(refresh, 10000);
