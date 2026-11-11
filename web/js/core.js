"use strict";
// ---------------------------------------------------------------- helpers --
const $ = (id) => document.getElementById(id);
const SVGNS = "http://www.w3.org/2000/svg";
function setAttrs(el, attrs) {
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.setAttribute("class", v);
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "value" || k === "checked" || k === "disabled" || k === "selected") el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
}
function addKids(el, kids) {
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}
// Views build nested arrays with nulls in them; make replaceChildren accept
// the same shapes as h() so every repaint goes through one code path.
const nativeReplace = Element.prototype.replaceChildren;
Element.prototype.replaceChildren = function (...kids) { nativeReplace.call(this); addKids(this, kids); };
function h(tag, attrs, ...kids) { const el = document.createElement(tag); setAttrs(el, attrs); return addKids(el, kids); }
function s(tag, attrs, ...kids) { const el = document.createElementNS(SVGNS, tag); setAttrs(el, attrs); return addKids(el, kids); }

const ICONS = {
  home: ["M3 11l9-8 9 8", "M5 10v10h14V10"],
  box: ["M3 7l9-4 9 4-9 4-9-4z", "M3 7v10l9 4 9-4V7", "M12 11v10"],
  play: ["M7 4l13 8-13 8V4z"],
  stop: ["M6 6h12v12H6z"],
  restart: ["M4 12a8 8 0 1 0 3-6.2", "M4 4v5h5"],
  list: ["M8 6h13", "M8 12h13", "M8 18h13", "M3 6h.01", "M3 12h.01", "M3 18h.01"],
  shield: ["M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6l8-3z"],
  activity: ["M3 12h4l3-8 4 16 3-8h4"],
  settings: ["M4 6h16", "M4 12h16", "M4 18h16", "M15 4v4", "M8 10v4", "M17 16v4"],
  plus: ["M12 5v14", "M5 12h14"],
  external: ["M14 4h6v6", "M20 4l-9 9", "M18 14v6H4V6h6"],
  git: ["M6 3v12", "M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M18 9a9 9 0 0 1-9 9"],
  clock: ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 7v5l3 2"],
  trash: ["M4 7h16", "M9 7V4h6v3", "M6 7l1 13h10l1-13"],
  copy: ["M9 9h11v11H9z", "M5 15H4V4h11v1"],
  download: ["M12 4v12", "M7 11l5 5 5-5", "M4 20h16"],
  moon: ["M20 14A8 8 0 1 1 10 4a6 6 0 0 0 10 10z"],
  chevron: ["M9 6l6 6-6 6"],
  pull: ["M12 3v13", "M7 11l5 5 5-5", "M5 21h14"],
};
function icon(name) { return s("svg", { class: "ic", viewBox: "0 0 24 24", "aria-hidden": "true" }, (ICONS[name] || []).map((d) => s("path", { d }))); }
function btn(label, onclick, cls, ic, extra) {
  return h("button", Object.assign({ class: "btn " + (cls || ""), type: "button", onclick }, extra || {}), ic ? icon(ic) : null, label);
}

function toast(msg, bad) {
  const t = h("div", { class: "toast" + (bad ? " bad" : "") }, msg);
  $("toasts").append(t);
  setTimeout(() => t.remove(), bad ? 7000 : 2800);
}
function confirmDlg(title, body, okLabel, danger) {
  return new Promise((resolve) => {
    const d = $("dlg");
    $("dlg-h").textContent = title;
    $("dlg-b").replaceChildren(...(Array.isArray(body) ? body : [body]));
    const yes = $("dlg-yes");
    yes.textContent = okLabel || "Confirm";
    yes.className = "btn " + (danger ? "danger" : "hl");
    const done = (v) => { d.close(); yes.onclick = $("dlg-no").onclick = d.oncancel = null; resolve(v); };
    yes.onclick = () => done(true);
    $("dlg-no").onclick = () => done(false);
    d.oncancel = () => done(false);
    d.showModal();
  });
}

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: body ? { "Content-Type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (_) { data = { error: text }; }
  // The session ended (or a password was just set elsewhere): show the sign-in page.
  if (res.status === 401 && !path.startsWith("/api/auth/")) { location.reload(); throw new Error("Sign in required"); }
  if (!res.ok) throw new Error((data && data.error) || res.status + " " + res.statusText);
  return data;
}
async function act(fn, okMsg) {
  try { await fn(); if (okMsg) toast(okMsg); } catch (e) { toast(e.message, true); }
  scheduleRefresh(60);
}

const parseUTC = (str) => (str ? new Date(str.replace(" ", "T") + "Z") : null);
const pad = (n) => String(n).padStart(2, "0");
function fmtDT(d) {
  if (!d || isNaN(d)) return "-";
  return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + " " + pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
}
const fmtClock = (d) => pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
function fmtAgo(d) {
  if (!d || isNaN(d)) return "-";
  let sec = Math.round((Date.now() - d.getTime()) / 1000);
  const future = sec < 0; sec = Math.abs(sec);
  let out;
  if (sec < 5) return "just now";
  if (sec < 60) out = sec + "s";
  else if (sec < 3600) out = Math.floor(sec / 60) + "m";
  else if (sec < 86400) out = Math.floor(sec / 3600) + "h " + Math.floor((sec % 3600) / 60) + "m";
  else out = Math.floor(sec / 86400) + "d " + Math.floor((sec % 86400) / 3600) + "h";
  return future ? "in " + out : out + " ago";
}
function when(d) { return h("span", { title: fmtDT(d), class: "nowrap" }, fmtAgo(d)); }
function fmtDur(ms) {
  if (ms === null || ms === undefined) return "-";
  if (ms < 1000) return ms + " ms";
  const sec = ms / 1000;
  if (sec < 60) return sec.toFixed(1) + " s";
  if (sec < 3600) return Math.floor(sec / 60) + "m " + Math.round(sec % 60) + "s";
  return Math.floor(sec / 3600) + "h " + Math.floor((sec % 3600) / 60) + "m";
}
function fmtUp(sec) {
  if (sec < 60) return sec + "s";
  if (sec < 3600) return Math.floor(sec / 60) + "m " + (sec % 60) + "s";
  if (sec < 86400) return Math.floor(sec / 3600) + "h " + Math.floor((sec % 3600) / 60) + "m";
  return Math.floor(sec / 86400) + "d " + Math.floor((sec % 86400) / 3600) + "h";
}
function fmtBytes(b) {
  if (b === null || b === undefined) return "-";
  if (b < 1024 * 1024) return (b / 1024).toFixed(0) + " KB";
  if (b < 1024 * 1024 * 1024) return (b / 1048576).toFixed(b < 10485760 ? 1 : 0) + " MB";
  return (b / 1073741824).toFixed(1) + " GB";
}
const short = (sha) => (sha ? sha.slice(0, 8) : "-");
const TRIGGERS = { MANUAL: "Manual", SYSTEM_AUTO: "Autostart", SCHEDULE: "Schedule", GIT_AUTO: "Auto-deploy", WEBHOOK: "Webhook", AUTO_RESTART: "Auto-restart", RESTART: "Restart", COMMAND: "Quick command", ROLLBACK: "Rollback" };
const trig = (x) => TRIGGERS[x] || x;
function copyText(text) {
  const done = () => toast("Copied to clipboard");
  if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text).then(done, () => toast("Copy failed", true));
  const ta = h("textarea", { style: "position:fixed;opacity:0" }); ta.value = text; document.body.append(ta); ta.select();
  try { document.execCommand("copy"); done(); } catch (_) { toast("Copy failed", true); }
  ta.remove();
}

// ------------------------------------------------------------------ state --
let targets = [], sys = null, activity = [], loaded = false, wsUp = false;
let authInfo = null;               // { password_set, authenticated, min_password_chars }
let refreshTimer = null;
const sinks = new Set();           // views listening to websocket messages
const ui = { projFilter: "", depStatus: "all", depTarget: "all", actLevel: "all" };
const T = (id) => targets.find((t) => t.id === Number(id));
const go = (path) => { location.hash = "#/" + path; };

function tStatus(t) {
  const hs = t.health && t.health.status;
  if (t.running) {
    if (t.run_mode !== "SERVICE") return { cls: "info", text: "Deploying", pulse: true };
    if (!t.health_url) return { cls: "ok", text: "Running" };
    if (hs === "LIVE") return { cls: "ok", text: "Running (healthy)" };
    if (hs === "SLOW") return { cls: "warn", text: "Running (slow)" };
    if (hs === "DOWN") return { cls: "warn", text: "Running (unhealthy)" };
    return { cls: "info", text: "Running (checking)", pulse: true };
  }
  if (t.restart_pending) return { cls: "warn", text: "Restarting", pulse: true };
  if (t.run_mode === "SERVICE") {
    if (hs === "LIVE" || hs === "SLOW") return { cls: "warn", text: "Up, not supervised" };
    if (t.last_run && t.last_run.status === "FAILED") return { cls: "err", text: "Exited (failed)" };
    return { cls: "off", text: "Stopped" };
  }
  if (!t.last_run) return { cls: "off", text: "Never deployed" };
  return t.last_run.status === "SUCCESS" ? { cls: "ok", text: "Deployed" } : { cls: "err", text: "Last deploy failed" };
}
function pill(st) { return h("span", { class: "pill " + st.cls }, h("span", { class: "dot " + (st.cls === "off" ? "" : st.cls) + (st.pulse ? " pulse" : "") }), st.text); }
function gitTag(t) {
  const g = t.git;
  if (!g) return h("span", { class: "tag" }, "git \u2026");
  if (!g.is_repo) return null;
  if (g.status === "UPDATES_AVAILABLE") return h("span", { class: "tag info", title: g.remote_summary || "" }, icon("git"), g.behind + " update" + (g.behind === 1 ? "" : "s"));
  if (g.status === "DIRTY") return h("span", { class: "tag warn", title: g.dirty_files + " changed file(s)" + (g.behind ? ", " + g.behind + " behind origin" : "") }, icon("git"), "dirty" + (g.behind ? " \u00b7 " + g.behind + " behind" : ""));
  return h("span", { class: "tag ok" }, icon("git"), "up to date");
}
function secTag(t) {
  if (t.scan_running) return h("span", { class: "tag info pulse" }, "scanning");
  if (!t.scan) return null;
  const n = t.scan.critical_issues_found + t.scan.vulnerable_packages_found;
  return h("span", { class: "tag " + (n ? "err" : "ok"), title: t.scan.critical_issues_found + " critical issue(s), " + t.scan.vulnerable_packages_found + " vulnerable package(s)" },
    icon("shield"), n ? n + " issue" + (n === 1 ? "" : "s") : "secure");
}
function runTag(status) {
  const cls = { SUCCESS: "ok", FAILED: "err", RUNNING: "info" }[status] || "";
  const text = { SUCCESS: "Success", FAILED: "Failed", RUNNING: "In progress" }[status] || status;
  return h("span", { class: "tag " + cls + (status === "RUNNING" ? " pulse" : "") }, text);
}
function strip(t, n) {
  const rec = (t.health_recent || []).slice(-(n || 30));
  return h("span", { class: "strip", title: "Last " + rec.length + " health probes (oldest first)" },
    rec.map((x) => h("i", { class: x.status, title: fmtClock(new Date(x.t * 1000)) + " " + x.status + (x.latency_ms !== null ? " " + x.latency_ms + " ms" : "") })));
}

// ---- actions on a target ----
const doRun = (t) => act(() => api("POST", "/api/targets/" + t.id + "/run"), t.run_mode === "SERVICE" ? "Starting\u2026" : "Deployment started");
const doStop = (t) => act(() => api("POST", "/api/targets/" + t.id + "/stop"), "Stopping\u2026");
const doRestart = (t) => act(() => api("POST", "/api/targets/" + t.id + "/restart"), "Restarting\u2026");
const doDeploy = (t) => act(() => api("POST", "/api/targets/" + t.id + "/deploy"), "Pulling and deploying\u2026");
const doScan = (t) => act(() => api("POST", "/api/targets/" + t.id + "/scan"), "Security scan started");
// "Roll back to this commit" for a commit the target has been on before.
// Hidden when it is the current commit or the folder is not a repository.
function rollbackButton(t, sha, small) {
  if (!t || !sha || !t.git || !t.git.is_repo || t.git.head === sha) return null;
  return btn("Roll back to this", async () => {
    const body = [h("p", {}, "The working tree of “" + t.name + "” is moved back to commit ", h("span", { class: "mono" }, short(sha)), " and the script is run" + (t.run_mode === "SERVICE" ? " (a running service is restarted)." : ".")),
      h("p", {}, "Only code is rolled back. Database migrations that newer commits applied are not reversed" + (t.db_file ? "; restore a database snapshot separately if you need to." : ".")),
      t.auto_deploy ? h("p", {}, "Auto-deploy is switched off, otherwise the next fetch would pull forward again.") : null,
      h("p", { class: "small" }, "Use Pull in Git Source to return to the latest commit.")];
    if (!(await confirmDlg("Roll back to " + short(sha) + "?", body, "Roll back", true))) return;
    try { const r = await api("POST", "/api/targets/" + t.id + "/rollback", { sha }); toast("Rolled back, run #" + r.run_id + " started"); await refresh(true); }
    catch (e) { toast(e.message, true); }
  }, "sm" + (small ? "" : " right"), "restart");
}
function actionButtons(t, small) {
  const sm = small ? " sm" : "";
  const behind = t.git && t.git.is_repo && t.git.behind > 0 && t.git.ahead === 0;
  const svc = t.run_mode === "SERVICE";
  const out = [];
  if (t.running) {
    if (svc) out.push(btn("Restart", () => doRestart(t), "warnc" + sm, "restart"));
    if (svc && behind) out.push(btn("Pull & Restart", () => doDeploy(t), sm, "pull"));
    out.push(btn("Stop", () => doStop(t), "danger" + sm, "stop"));
  } else {
    out.push(btn(svc ? "Start" : (t.last_run ? "Redeploy" : "Deploy"), () => doRun(t), "hl" + sm, "play"));
    if (behind) out.push(btn(svc ? "Pull & Start" : "Pull & Deploy", () => doDeploy(t), "warnc" + sm, "pull"));
    if (t.restart_pending) out.push(btn("Cancel restart", () => doStop(t), sm));
  }
  return out;
}

