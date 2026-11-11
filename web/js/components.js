"use strict";
// ----------------------------------------------------------------- charts --
// Single-series area/line chart with a crosshair tooltip. points: [{t (unix s), v}].
function lineChart(points, o) {
  o = o || {};
  const fmt = o.fmt || ((v) => String(Math.round(v)));
  const wrap = h("div", { class: "chart", style: o.height ? "height:" + o.height + "px" : null });
  if (!points.length) { wrap.append(h("div", { class: "none" }, o.empty || "No samples yet")); return wrap; }
  const maxV = Math.max(...points.map((p) => p.v));
  let top = o.max || 0;
  if (!top) { const mag = Math.pow(10, Math.floor(Math.log10(maxV || 1))); top = Math.ceil((maxV || 1) / mag) * mag; if (top / 2 >= maxV && top > 1) top /= 2; }
  if (o.minTop) top = Math.max(top, o.minTop);
  const t0 = points[0].t, t1 = points[points.length - 1].t, span = Math.max(1, t1 - t0);
  const X = (p) => ((p.t - t0) / span) * 100, Y = (p) => 100 - Math.min(100, (p.v / top) * 100);
  [0, 50, 100].forEach((pc) => {
    wrap.append(h("div", { class: "gl", style: "top:" + pc + "%" }), h("div", { class: "yl", style: "top:" + pc + "%" }, fmt(top * (1 - pc / 100))));
  });
  const line = points.map((p, i) => (i ? "L" : "M") + X(p).toFixed(2) + " " + Y(p).toFixed(2)).join(" ");
  const single = points.length === 1;
  wrap.append(s("svg", { viewBox: "0 0 100 100", preserveAspectRatio: "none", role: "img", "aria-label": o.label || "chart" },
    single ? null : s("path", { d: line + " L100 100 L0 100 Z", fill: "var(--series)", "fill-opacity": ".12", stroke: "none" }),
    single ? null : s("path", { d: line, fill: "none", stroke: "var(--series)", "stroke-width": "2", "vector-effect": "non-scaling-stroke", "stroke-linejoin": "round" })));
  wrap.append(h("div", { class: "xl", style: "left:0" }, fmtClock(new Date(t0 * 1000))), h("div", { class: "xl", style: "right:0" }, fmtClock(new Date(t1 * 1000))));
  const cross = h("div", { class: "cross" }), pt = h("div", { class: "pt" }), tip = h("div", { class: "tip" });
  wrap.append(cross, pt, tip);
  if (single) { pt.style.display = "block"; pt.style.left = "0%"; pt.style.top = Y(points[0]) + "%"; }
  wrap.addEventListener("mousemove", (e) => {
    const r = wrap.getBoundingClientRect();
    const tt = t0 + ((e.clientX - r.left) / r.width) * span;
    let best = points[0];
    for (const p of points) if (Math.abs(p.t - tt) < Math.abs(best.t - tt)) best = p;
    const x = X(best);
    cross.style.left = pt.style.left = tip.style.left = x + "%";
    pt.style.top = Y(best) + "%";
    tip.replaceChildren(h("b", {}, fmt(best.v)), "  " + fmtClock(new Date(best.t * 1000)));
    cross.style.display = pt.style.display = tip.style.display = "block";
  });
  wrap.addEventListener("mouseleave", () => { cross.style.display = tip.style.display = "none"; if (!single) pt.style.display = "none"; });
  return wrap;
}
function chartBlock(title, nowText, points, o) {
  return h("div", {}, h("div", { class: "chart-h" }, h("b", {}, title), h("span", { class: "dim small" }, (o && o.note) || ""), h("span", { class: "now" }, nowText)), lineChart(points, o));
}

// -------------------------------------------------------------- log viewer --
function guessStream(text) { return /^\[(portalcontrol|scan)\]/.test(text) ? "system" : "stdout"; }
function LogView(name) {
  const lines = [];
  let filter = "", streamSel = "all";
  const pre = h("pre", { class: "log", tabindex: "0" });
  const count = h("span", { class: "dim small right" });
  const follow = h("input", { type: "checkbox", checked: true });
  const wrapBox = h("input", { type: "checkbox", checked: true, onchange: () => pre.classList.toggle("nowrap", !wrapBox.checked) });
  const passes = (l) => (streamSel === "all" || l.stream === streamSel) && (!filter || l.text.toLowerCase().includes(filter));
  function node(l) {
    const row = h("div", { class: l.stream }, l.ts ? h("span", { class: "ts" }, fmtClock(new Date(l.ts))) : null,
      l.tag ? h("span", { class: "ptag c" + (l.tag.color % 6) }, l.tag.label) : null);
    if (filter) {
      const i = l.text.toLowerCase().indexOf(filter);
      row.append(l.text.slice(0, i), h("mark", {}, l.text.slice(i, i + filter.length)), l.text.slice(i + filter.length) || "");
    } else row.append(l.text || " ");
    return row;
  }
  function stats(shown) { count.textContent = (filter || streamSel !== "all" ? shown + " of " : "") + lines.length + " line" + (lines.length === 1 ? "" : "s"); }
  function redraw() {
    const vis = lines.filter(passes);
    pre.replaceChildren(...vis.map(node));
    stats(vis.length);
    if (follow.checked) pre.scrollTop = pre.scrollHeight;
  }
  function add(text, stream, ts, tag) {
    const l = { text: text || "", stream: stream || guessStream(text || ""), ts, tag };
    lines.push(l);
    if (lines.length > 6000) { lines.splice(0, 1000); redraw(); return; }
    if (passes(l)) { pre.append(node(l)); if (follow.checked) pre.scrollTop = pre.scrollHeight; }
    stats(pre.childNodes.length);
  }
  function setText(text) {
    lines.length = 0;
    if (text) text.replace(/\n$/, "").split("\n").forEach((x) => lines.push({ text: x, stream: guessStream(x) }));
    redraw();
  }
  const plain = () => lines.filter(passes).map((l) => (l.tag ? "[" + l.tag.label + "] " : "") + l.text).join("\n");
  const search = h("input", { type: "search", placeholder: "Filter lines\u2026", "aria-label": "Filter log lines",
    oninput: () => { filter = search.value.trim().toLowerCase(); redraw(); } });
  const sel = h("select", { "aria-label": "Stream", onchange: () => { streamSel = sel.value; redraw(); } },
    h("option", { value: "all" }, "All output"), h("option", { value: "stdout" }, "stdout"), h("option", { value: "stderr" }, "stderr"), h("option", { value: "system" }, "PortalControl"));
  const el = h("div", { class: "logbox" },
    h("div", { class: "logbar" }, search, sel, h("label", {}, follow, "Follow"), h("label", {}, wrapBox, "Wrap"), count,
      btn("Copy", () => copyText(plain()), "ghost sm", "copy"),
      btn("Download", () => {
        const a = h("a", { href: URL.createObjectURL(new Blob([plain() + "\n"], { type: "text/plain" })), download: (name || "log") + ".log" });
        document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(a.href), 2000);
      }, "ghost sm", "download")),
    pre);
  redraw();
  return { el, add, setText };
}

// -------------------------------------------------------------- small UI --
function field(label, input, hint) { return h("label", { class: "f" }, h("span", {}, label), input, hint ? h("div", { class: "hint" }, hint) : null); }
function check(input, title, desc) { return h("label", { class: "check" }, input, h("span", {}, h("b", {}, title), h("small", {}, desc))); }
function facts(rows) { return h("dl", { class: "facts" }, rows.filter(Boolean).map(([k, v]) => [h("dt", {}, k), h("dd", {}, v === null || v === undefined || v === "" ? "-" : v)])); }
function table(headers, rows, onRow) {
  return h("div", { class: "tblwrap" }, h("table", { class: "tbl" }, h("thead", {}, h("tr", {}, headers.map((x) => h("th", {}, x)))),
    h("tbody", {}, rows.map((r, i) => h("tr", { class: onRow ? "click" : null, onclick: onRow ? () => onRow(i) : null }, r.map((x) => h("td", {}, x)))))));
}
function empty(title, text, action) { return h("div", { class: "empty" }, h("b", {}, title), text, action ? h("div", { style: "margin-top:14px" }, action) : null); }
function tile(label, value, note, cls) {
  return h("div", { class: "box tile" }, h("div", { class: "lbl" }, label), h("div", { class: "val", style: cls ? "color:var(--" + cls + ")" : null }, value), note ? h("div", { class: "note" }, note) : null);
}
function depRow(r, showTarget) {
  const t = T(r.target_id);
  const title = showTarget ? (r.target_name || (t && t.name) || "#" + r.target_id) + (r.label ? " — " + r.label : "")
    : (r.label || r.commit_message || (r.commit_sha ? "Commit " + short(r.commit_sha) : "Run #" + r.id));
  return h("div", { class: "dep " + r.status, onclick: () => go("target/" + r.target_id + "/deployments/" + r.id), role: "link", tabindex: "0",
      onkeydown: (e) => { if (e.key === "Enter") go("target/" + r.target_id + "/deployments/" + r.id); } },
    h("div", {}, runTag(r.status)),
    h("div", { style: "min-width:0" }, h("div", { class: "l1" }, title),
      h("div", { class: "l2" }, "#" + r.id + " \u00b7 " + trig(r.triggered_by),
        r.commit_sha ? [" \u00b7 ", h("span", { class: "mono" }, short(r.commit_sha))] : null,
        showTarget && r.commit_message ? " \u00b7 " + r.commit_message : null,
        r.exit_code !== null && r.exit_code !== 0 ? " \u00b7 exit " + r.exit_code : null)),
    h("div", { class: "r" }, h("div", {}, when(parseUTC(r.executed_at))), h("div", {}, r.status === "RUNNING" ? "running" : fmtDur(r.duration_ms))));
}
function feedRow(e, withTarget) {
  const t = e.target_id ? T(e.target_id) : null;
  const cls = { ok: "ok", warn: "warn", error: "err", info: "info" }[e.level] || "";
  return h("div", { class: "feed" }, h("span", { class: "dot " + cls, style: "position:relative;top:-1px" }),
    h("div", {}, withTarget && t ? [h("a", { href: "#/target/" + t.id, style: "font-weight:600" }, t.name), " \u2014 "] : null, e.message),
    h("span", { class: "when" }, when(new Date(e.ts))));
}

