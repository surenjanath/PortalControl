"use strict";
// ---------------------------------------------- live monitor pipeline view --
// A live diagram of monitor.rs: every output line flows from its process into
// the filter; ordinary lines drop into PASS, error lines go on to the checks;
// a rule hit carries its trace id to the AI model, then to the incident, the
// email and the person who applies the fix. Driven only by real events.

const FLOW_W = 1000, FLOW_H = 470;
const FLOW_SRC_MAX = 5;

function flowBox(g, x, y, w, hgt, title, sub, cls) {
  const box = s("g", { class: "fbox " + (cls || "") });
  box.append(s("rect", { x, y, width: w, height: hgt }));
  const t = s("text", { x: x + w / 2, y: y + hgt / 2 - 3, class: "ft" }); t.textContent = title;
  const st = s("text", { x: x + w / 2, y: y + hgt / 2 + 14, class: "fs" }); st.textContent = sub || "";
  box.append(t, st); g.append(box);
  return { box, title: t, sub: st, cx: x + w / 2, cy: y + hgt / 2 };
}
function flowCyl(g, x, y, w, hgt, title, sub) {
  const r = 9, box = s("g", { class: "fbox" });
  box.append(s("path", { d: `M${x},${y + r} a${w / 2},${r} 0 0 1 ${w},0 v${hgt - 2 * r} a${w / 2},${r} 0 0 1 -${w},0 Z` }),
    s("path", { d: `M${x},${y + r} a${w / 2},${r} 0 0 0 ${w},0`, class: "cyl-top" }));
  const t = s("text", { x: x + w / 2, y: y + hgt / 2 + 1, class: "ft" }); t.textContent = title;
  const st = s("text", { x: x + w / 2, y: y + hgt / 2 + 17, class: "fs" }); st.textContent = sub || "";
  box.append(t, st); g.append(box);
  return { box, title: t, sub: st };
}
function flowCont(g, x, y, w, hgt, label) {
  g.append(s("rect", { x, y, width: w, height: hgt, class: "fcont" }));
  const t = s("text", { x: x + 10, y: y + 15, class: "fl" }); t.textContent = label; g.append(t);
}
function flowNum(g, x, y, n) {
  g.append(s("circle", { cx: x, cy: y, r: 10, class: "fnum" }));
  const t = s("text", { x, y: y + 4, class: "fnt" }); t.textContent = n; g.append(t);
}
function flowPath(g, d, cls, id) {
  const p = s("path", { d, class: "fpath " + (cls || ""), "marker-end": cls && cls.includes("noarrow") ? null : "url(#fa)" });
  if (id) p.dataset.k = id;
  g.append(p);
  return p;
}

function monitorFlow() {
  const reduce = window.matchMedia && matchMedia("(prefers-reduced-motion: reduce)").matches;
  const status = h("div", { class: "flow-status" });
  const svg = s("svg", { viewBox: `0 0 ${FLOW_W} ${FLOW_H}`, class: "flow-svg", role: "img", "aria-label": "Live monitor pipeline" });
  const defs = s("defs");
  const marker = s("marker", { id: "fa", viewBox: "0 0 10 10", refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: "auto-start-reverse" });
  marker.append(s("path", { d: "M0,0 L10,5 L0,10 z", class: "farrow" }));
  defs.append(marker); svg.append(defs);
  const g = s("g"); svg.append(g);
  const top = s("g"); svg.append(top); // particles above everything

  // ---- containers
  flowCont(g, 12, 52, 176, 348, "SOURCES · LIVE OUTPUT");
  flowCont(g, 220, 30, 404, 396, "STAGE 1 · RULES + LEARNED PATTERNS · µS PER LINE");
  flowCont(g, 652, 30, 336, 396, "STAGE 2 · LOCAL AI · FLAGGED ONLY");

  // ---- stage boxes
  const filter = flowBox(g, 250, 170, 130, 52, "FILTER", "ERROR WORDS");
  const pass = flowCyl(g, 250, 318, 130, 60, "PASS", "0 DROPPED");
  const burst = flowBox(g, 462, 70, 140, 46, "BURST", "10+/MIN · 4× BASELINE");
  const novel = flowBox(g, 462, 150, 140, 46, "NEW PATTERN", "LEARNING");
  const events = flowBox(g, 462, 230, 140, 46, "EVENTS", "FAIL · CRASH · DOWN");
  const ai = flowBox(g, 680, 140, 160, 62, "AI MODEL", "—", "ai");
  const aiNote = s("text", { x: 760, y: 222, class: "fs fnote" }); g.append(aiNote);
  const inc = flowBox(g, 680, 274, 160, 56, "INCIDENT", "0 OPEN");
  const mail = flowBox(g, 864, 274, 110, 56, "EMAIL", "0 SENT");
  const you = flowBox(g, 864, 140, 110, 62, "YOU", "APPLY FIX", "dashed");
  const flagged = s("text", { x: 636, y: 158, class: "fs" }); g.append(flagged);
  flowNum(g, 100, 40, "1"); flowNum(g, 315, 150, "2"); flowNum(g, 532, 50, "3"); flowNum(g, 760, 120, "4"); flowNum(g, 919, 120, "5");

  // ---- paths
  const P = {};
  P.pass = flowPath(g, "M315,222 V318");
  P.burst = flowPath(g, "M380,196 H425 V93 H462");
  P.novel = flowPath(g, "M425,173 H462");
  P.events = flowPath(g, "M100,400 V412 H440 V253 H462", "event");
  P.fromBurst = flowPath(g, "M602,93 H636 V171 H680");
  P.fromNovel = flowPath(g, "M602,173 H636 V171 H680");
  P.fromEvents = flowPath(g, "M602,253 H636 V171 H680");
  P.aiInc = flowPath(g, "M760,202 V274");
  P.incMail = flowPath(g, "M840,302 H864");
  P.mailYou = flowPath(g, "M919,274 V202", "event");
  P.back = flowPath(g, "M919,140 V16 H100 V30", "event");
  g.append(s("circle", { cx: 636, cy: 171, r: 4.5, class: "fjoin" }));
  const backLabel = s("text", { x: 520, y: 12, class: "fl" }); backLabel.textContent = "FIX APPLIED → RESTART · MIGRATE · ROLL BACK · PULL — SAME TRACE ID"; g.append(backLabel);
  const evLabel = s("text", { x: 230, y: 425, class: "fl" }); evLabel.textContent = "ACTIVITY EVENTS"; g.append(evLabel);

  // ---- legend
  const lg = s("g", { class: "flegend" });
  lg.append(s("rect", { x: 230, y: 446, width: 14, height: 10, class: "fcomp" }));
  const l1 = s("text", { x: 250, y: 455, class: "fl" }); l1.textContent = "COMPONENT";
  lg.append(s("line", { x1: 340, y1: 451, x2: 374, y2: 451, class: "fpath" }));
  const l2 = s("text", { x: 380, y: 455, class: "fl" }); l2.textContent = "OUTPUT LINE";
  lg.append(s("line", { x1: 470, y1: 451, x2: 504, y2: 451, class: "fpath event" }));
  const l3 = s("text", { x: 510, y: 455, class: "fl" }); l3.textContent = "EVENT";
  lg.append(s("circle", { cx: 572, cy: 451, r: 4, class: "pt err" }));
  const l4 = s("text", { x: 582, y: 455, class: "fl" }); l4.textContent = "FLAGGED, WITH ITS TRACE ID";
  lg.append(l1, l2, l3, l4); g.append(lg);

  // ---- sources (re-laid when the set of running targets changes)
  const srcG = s("g"); g.append(srcG);
  let srcKeys = "", srcIndex = new Map(), srcBoxes = [], srcPaths = [];
  const rate = new Map(); // target id -> recent line timestamps
  // A target that printed in the last 10 s is live, even before the target
  // list (polled) says it is running.
  const live = (t) => t.running || (rate.get(t.id) || []).some((x) => Date.now() - x < 10000);
  const layoutSources = () => {
    // Only FLOW_SRC_MAX boxes fit: show the targets that print the most, so
    // the source of a flood of lines is never one of the hidden ones.
    const order = new Map(targets.map((t, i) => [t.id, i]));
    const list = targets.slice()
      .sort((a, b) => (live(b) - live(a)) || (volume(b) - volume(a)) || (order.get(a.id) - order.get(b.id)))
      .slice(0, FLOW_SRC_MAX)
      .sort((a, b) => (live(b) - live(a)) || (order.get(a.id) - order.get(b.id)));
    const hidden = targets.length - list.length;
    // Re-lay when a target starts or stops too, not only when the set changes.
    // Lines of a target that has no box (more targets than boxes, or a
    // process whose target is not in the list) get a box of their own, so
    // they are never drawn as coming out of another target.
    const other = hidden > 0 || otherRate.some((x) => Date.now() - x < 60000);
    const key = list.map((t) => t.id + (live(t) ? "r" : "s")).join(",") + (other ? ",+" + hidden : "");
    if (key === srcKeys) return;
    srcKeys = key; srcG.replaceChildren(); srcIndex = new Map(); srcBoxes = []; srcPaths = []; otherBox = null; otherPath = null;
    if (!list.length && !other) { const t = s("text", { x: 100, y: 220, class: "fs" }); t.textContent = "NO TARGETS YET"; srcG.append(t); return; }
    const step = other ? 56 : 64, hgt = other ? 42 : 46, y0 = other ? 62 : 70;
    list.forEach((t, i) => {
      const y = y0 + i * step;
      const name = t.name.length > 16 ? t.name.slice(0, 15) + "…" : t.name;
      srcBoxes.push(flowBox(srcG, 26, y, 148, hgt, name.toUpperCase(), live(t) ? "LIVE" : "STOPPED", live(t) ? "" : "dim"));
      srcPaths.push(flowPath(srcG, `M174,${y + hgt / 2} H205 V196 H250`));
      srcIndex.set(t.id, i);
    });
    if (other) {
      const y = y0 + list.length * step;
      otherBox = flowBox(srcG, 26, y, 148, hgt, hidden > 0 ? "+" + hidden + " OTHER" : "NOT IN THE LIST", "", "dashed");
      otherPath = flowPath(srcG, `M174,${y + hgt / 2} H205 V196 H250`);
    }
  };
  let otherBox = null, otherPath = null, otherRate = [];
  const volume = (t) => (st.targets && st.targets[t.id] ? st.targets[t.id].lines : 0);

  // ---- where the lines come from: every target, busiest first
  const bySource = h("div", { class: "flow-src" });
  const paintSources = () => {
    const now = Date.now(), n = (x) => Number(x || 0).toLocaleString();
    const rows = Object.entries(st.targets || {}).map(([id, x]) => ({ id: Number(id), t: T(id), x }))
      .sort((a, b) => b.x.lines - a.x.lines);
    if (!rows.length) { bySource.replaceChildren(); return; }
    const total = rows.reduce((sum, r) => sum + r.x.lines, 0) || 1;
    const shown = rows.slice(0, 12);
    bySource.replaceChildren(
      h("div", { class: "dim small", style: "margin:12px 0 6px" }, h("b", {}, "Where the lines come from"), " · counted since the manager started · click a row to open the target and its log"),
      table(["Target", "Lines", "Share", "Error lines", "Patterns", "Now"], shown.map((r) => [
        r.t ? r.t.name + (r.t.project ? " (" + r.t.project + ")" : "") : "target " + r.id,
        n(r.x.lines), (100 * r.x.lines / total).toFixed(1) + "%", n(r.x.error_lines), n(r.x.patterns),
        ((rate.get(r.id) || []).filter((ts) => now - ts < 10000).length / 10).toFixed(1) + " lines/s",
      ]), (i) => { location.hash = "#/target/" + shown[i].id; }),
      rows.length > shown.length ? h("div", { class: "dim small", style: "margin-top:4px" }, "and " + (rows.length - shown.length) + " quieter targets") : null);
  };

  // ---- particles
  const parts = [];
  let raf = 0, spawnedThisSecond = 0, secondStart = 0;
  const run = (path, dur, cls, label, done) => {
    if (reduce || !path || !path.isConnected) { if (done) done(); return; }
    const c = s("circle", { r: label ? 5 : 3.4, class: "pt " + (cls || "") });
    let tx = null;
    if (label) { tx = s("text", { class: "ptl" }); tx.textContent = label; top.append(tx); }
    top.append(c);
    parts.push({ path, len: path.getTotalLength(), t0: performance.now(), dur, c, tx, done });
    if (!raf) raf = requestAnimationFrame(tick);
  };
  const tick = (now) => {
    for (let i = parts.length - 1; i >= 0; i--) {
      const p = parts[i], k = Math.min(1, (now - p.t0) / p.dur);
      const e = k < 0.5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;
      const pt = p.path.getPointAtLength(e * p.len);
      p.c.setAttribute("cx", pt.x); p.c.setAttribute("cy", pt.y);
      if (p.tx) { p.tx.setAttribute("x", pt.x + 8); p.tx.setAttribute("y", pt.y - 8); }
      if (k >= 1) { p.c.remove(); if (p.tx) p.tx.remove(); parts.splice(i, 1); if (p.done) p.done(); }
    }
    raf = parts.length ? requestAnimationFrame(tick) : 0;
  };
  const flash = (b, cls) => { b.box.classList.remove(cls || "hit"); void b.box.getBBox(); b.box.classList.add(cls || "hit"); };

  // ---- counters
  let st = { lines: 0, error_lines: 0, flagged: 0, ai_calls: 0, ai_avg_ms: null, last_ai_ms: null, emails: 0, open_incidents: 0, model: "", ai_enabled: false, enabled: true, last_trace: null, targets: {} };
  let lastTrace = "", aiResult = "";
  const isErr = (l) => /error|exception|traceback|fatal|panic|critical|segmentation fault|out of memory|unhandled|refused|timed out|timeout|denied|failed|failure|killed/i.test(l)
    && !/\b0 (errors?|failed)\b|errors?[=:] ?0\b|no errors/i.test(l) || /" 5\d\d /.test(l);
  const paint = () => {
    const pct = st.lines ? (100 * (st.lines - st.error_lines)) / st.lines : 100;
    pass.sub.textContent = (st.lines - st.error_lines) + " DROPPED · " + pct.toFixed(1) + "%";
    filter.sub.textContent = st.error_lines + " ERROR LINE" + (st.error_lines === 1 ? "" : "S");
    const learning = Object.values(st.targets || {});
    const learnedPatterns = learning.reduce((n, x) => n + x.patterns, 0);
    novel.sub.textContent = learning.some((x) => x.learning) ? "LEARNING · " + learnedPatterns + " PATTERNS" : learnedPatterns + " PATTERNS KNOWN";
    flagged.textContent = st.flagged ? st.flagged + " FLAGGED" : "";
    ai.sub.textContent = st.ai_enabled ? (st.model || "no model").toUpperCase() : "OFF · RULES ONLY";
    ai.box.classList.toggle("off", !st.ai_enabled);
    if (!ai.box.classList.contains("thinking")) aiNote.textContent = !st.ai_enabled ? "" : aiResult ? aiResult : (st.ai_avg_ms ? "AVG " + (st.ai_avg_ms / 1000).toFixed(1) + " S · " + st.ai_calls + " CALL" + (st.ai_calls === 1 ? "" : "S") : "IDLE · WAITING FOR A FLAG");
    inc.sub.textContent = st.open_incidents + " OPEN" + (lastTrace ? " · " + lastTrace.slice(-4).toUpperCase() : "");
    mail.sub.textContent = st.emails + " SENT";
    const now = Date.now();
    srcBoxes.forEach((b, i) => {
      const id = parseInt(srcKeys.split(",")[i], 10); const t = T(id);
      const ts = (rate.get(id) || []).filter((x) => now - x < 10000); rate.set(id, ts);
      const info = st.targets && st.targets[id];
      b.sub.textContent = t && live(t) ? (ts.length / 10).toFixed(1) + " LINES/S" + (info ? " · " + info.lines : "") : "STOPPED";
    });
    otherRate = otherRate.filter((x) => now - x < 60000);
    if (otherBox) otherBox.sub.textContent = (otherRate.filter((x) => now - x < 10000).length / 10).toFixed(1) + " LINES/S · SEE TABLE";
    status.replaceChildren(
      h("span", { class: "dot " + (st.enabled ? "ok pulse" : "") }), h("b", {}, st.enabled ? " LIVE " : " OFF "),
      h("span", {}, h("b", {}, st.lines), " lines since start · ", h("b", {}, pct.toFixed(1) + "%"), " filtered · ", h("b", {}, st.flagged), " flagged · "),
      h("span", {}, "AI ", h("b", {}, st.ai_enabled ? st.model || "—" : "off"), st.ai_avg_ms ? " · avg " + (st.ai_avg_ms / 1000).toFixed(1) + " s" : ""),
      st.ai_enabled ? h("span", { class: "t-ok" }, " local") : null,
      h("span", { class: "right mono" }, lastTrace ? lastTrace + "  " : "", fmtClock(new Date())));
  };
  const loadStats = () => api("GET", "/api/monitor/stats").then((x) => { st = x; if (!lastTrace && x.last_trace) lastTrace = x.last_trace; layoutSources(); paint(); paintSources(); }, () => {});

  // ---- live events
  const pathFor = (trigger) => trigger === "error_burst" ? [P.burst, burst, P.fromBurst] : trigger === "new_error" ? [P.novel, novel, P.fromNovel] : [P.events, events, P.fromEvents];
  const sink = (m) => {
    if (m.type === "log" && m.target_id) {
      const line = m.line || "";
      if (line.startsWith("[portalcontrol]") || line.startsWith("[scan]")) return;
      const err = isErr(line);
      st.lines++; if (err) st.error_lines++;
      const ts = rate.get(m.target_id) || []; ts.push(Date.now()); rate.set(m.target_id, ts);
      const now = performance.now();
      if (now - secondStart > 1000) { secondStart = now; spawnedThisSecond = 0; }
      if (spawnedThisSecond++ > 24 && !err) return; // sample busy streams; errors always show
      const drawn = srcIndex.has(m.target_id);
      if (!drawn) { otherRate.push(Date.now()); if (!otherPath) layoutSources(); }
      const path = drawn ? srcPaths[srcIndex.get(m.target_id)] : otherPath;
      run(path, 900, err ? "warn" : "", null, () => {
        flash(filter);
        if (!err) run(P.pass, 550, "", null, () => flash(pass));
        else { const toBurst = Math.random() < 0.5; run(toBurst ? P.burst : P.novel, 650, "warn", null, () => flash(toBurst ? burst : novel, "hitwarn")); }
      });
    } else if (m.type === "monitor") {
      if (m.stage === "flagged") {
        st.flagged++; lastTrace = m.trace || lastTrace;
        const [into, box, out] = pathFor(m.trigger);
        if (m.trigger !== "error_burst" && m.trigger !== "new_error") run(into, 1200, "err", null, () => flash(box, "hiterr"));
        else flash(box, "hiterr");
        run(out, 1400, "err", (m.trace || "").slice(-4).toUpperCase(), () => flash(ai, "hiterr"));
      } else if (m.stage === "ai_start") {
        ai.box.classList.add("thinking"); aiNote.textContent = "THINKING · " + (m.trace || "");
      } else if (m.stage === "ai_done") {
        ai.box.classList.remove("thinking");
        if (m.ok) { st.ai_calls++; st.last_ai_ms = m.ms; aiResult = (m.trace || "").slice(-4).toUpperCase() + " · " + (m.ms / 1000).toFixed(1) + " S · " + String(m.action || "none").replace("command:", "").toUpperCase(); }
        else aiResult = "MODEL UNAVAILABLE · RULES ONLY";
      } else if (m.stage === "email") {
        if (m.ok) st.emails++;
        run(P.incMail, 700, "accent", (m.trace || "").slice(-4).toUpperCase(), () => { flash(mail); run(P.mailYou, 800, "accent", null, () => flash(you)); });
      }
    } else if (m.type === "incident" && m.trace) {
      lastTrace = m.trace; st.open_incidents++;
      run(P.aiInc, 800, "err", m.trace.slice(-4).toUpperCase(), () => flash(inc, "hiterr"));
    } else if (m.type === "activity" && m.kind === "ai" && /Applied/.test(m.message || "")) {
      run(P.back, 1800, "ok", "FIX", () => {});
    }
    paint();
  };
  sinks.add(sink);
  layoutSources(); loadStats();
  const timer = setInterval(() => { layoutSources(); loadStats(); }, 5000);
  const clock = setInterval(() => { layoutSources(); paint(); }, 1000);
  const el = h("div", { class: "flow" }, status, h("div", { class: "flow-wrap" }, svg), bySource,
    h("div", { class: "flow-cap" }, h("b", {}, "Figure. "), "Every line is checked on this machine in microseconds; ordinary output drops into PASS. Only flagged moments reach the AI model, each with its own trace id, so an alert can be followed back to the exact log lines."));
  return { el, cleanup: () => { sinks.delete(sink); clearInterval(timer); clearInterval(clock); cancelAnimationFrame(raf); } };
}

// ---- trace view: one incident's chain, from log lines to fix ----
async function openTrace(traceId) {
  let d;
  try { d = await api("GET", "/api/traces/" + encodeURIComponent(traceId)); } catch (e) { return toast(e.message, true); }
  const tr = d.trace, i = d.incident, t = d.target;
  const step = (n, title, body, cls) => h("div", { class: "tstep " + (cls || "") }, h("span", { class: "tn" }, n), h("div", { style: "flex:1;min-width:0" }, h("b", {}, title), body));
  const pre = (txt) => h("pre", { class: "log", style: "max-height:220px;margin-top:6px;white-space:pre-wrap" }, txt);
  const dlg = h("dialog", { style: "width:min(820px,95vw)" },
    h("div", { class: "dh" }, "Trace ", h("span", { class: "mono" }, tr.trace_id)),
    h("div", { style: "padding:4px 18px 16px;max-height:72vh;overflow:auto" },
      step("1", "Log lines", h("div", { class: "muted small" }, t ? t.name + " (project " + t.project + ")" : "removed target", tr.run_id ? [" · ", h("a", { href: "#/target/" + tr.target_id + "/deployments/" + tr.run_id, onclick: () => dlg.close() }, "open the log of run #" + tr.run_id)] : "",
        i && i.excerpt ? h("details", {}, h("summary", { class: "dim small", style: "cursor:pointer" }, "the lines that were looked at"), pre(i.excerpt)) : null)),
      step("2", "Rule", h("div", { class: "muted small" }, tr.trigger.replace("_", " ") + " — " + tr.what + " · " + fmtDT(parseUTC(tr.created_at)))),
      step("3", "AI model", tr.status === "rules" ? h("div", { class: "muted small" }, "Not asked: AI analysis was off.")
        : h("div", { class: "small" }, h("div", { class: tr.status === "ok" ? "" : "t-err" }, tr.model + " on this machine · " + (tr.status === "ok" ? (tr.duration_ms / 1000).toFixed(1) + " s · " + (tr.prompt_tokens || "?") + " tokens in, " + (tr.answer_tokens || "?") + " out" : "failed: " + tr.error)),
          h("details", {}, h("summary", { class: "dim small", style: "cursor:pointer" }, "prompt sent to the model"), pre(tr.prompt)),
          tr.answer ? h("details", {}, h("summary", { class: "dim small", style: "cursor:pointer" }, "raw answer"), pre(tr.answer)) : null), tr.status === "failed" ? "bad" : ""),
      step("4", "Incident", i ? h("div", { class: "small" }, "#" + i.id + " · " + i.severity + " · " + i.title + " · " + i.status, i.action !== "none" ? h("div", { class: "muted" }, "Suggested: " + i.action_label) : null) : h("div", { class: "muted small" }, "—")),
      step("5", "Email", h("div", { class: "muted small" }, tr.email || "no alert recorded")),
      step("6", "Fix", h("div", { class: "muted small" }, tr.applied || "not applied yet"), tr.applied ? "good" : "")),
    h("div", { class: "df" }, btn("Copy trace id", () => copyText(tr.trace_id), "", "copy"), btn("Close", () => dlg.close(), "hl")));
  dlg.addEventListener("close", () => dlg.remove());
  document.body.append(dlg); dlg.showModal();
}
