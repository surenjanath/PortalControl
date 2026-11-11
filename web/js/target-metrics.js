"use strict";
// ---- metrics tab ----
// Three ranges: "Live" (5-second samples, last 15 minutes, held in memory)
// and "24 hours" / "7 days" (one-minute rows from the database, averaged
// into buckets). Deployment statistics come from the run history.

const fmtPct = (v) => (v < 10 ? v.toFixed(1) : Math.round(v)) + "%";
const fmtRate = (v) => fmtBytes(v) + "/s";
const fmtMs = (v) => Math.round(v) + " ms";
const fmtStamp = (d) => pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + " " + pad(d.getHours()) + ":" + pad(d.getMinutes());
function percentile(sorted, p) { return sorted.length ? sorted[Math.min(sorted.length - 1, Math.round((sorted.length - 1) * p))] : null; }

// Figures derived from the run history (most recent first).
function deployStats(runs) {
  const done = runs.filter((r) => r.status !== "RUNNING" && !r.label);
  const ok = done.filter((r) => r.status === "SUCCESS");
  const durations = done.map((r) => r.duration_ms || 0).sort((a, b) => a - b);
  const weekAgo = Date.now() - 7 * 86400000;
  const days = [];
  for (let i = 13; i >= 0; i--) {
    const d = new Date(); d.setHours(0, 0, 0, 0); d.setDate(d.getDate() - i);
    days.push({ day: d, ok: 0, failed: 0 });
  }
  done.forEach((r) => {
    const at = parseUTC(r.executed_at); if (!at) return;
    const slot = days.find((x) => at >= x.day && at < new Date(x.day.getTime() + 86400000));
    if (slot) slot[r.status === "SUCCESS" ? "ok" : "failed"]++;
  });
  return {
    total: done.length, ok: ok.length, failed: done.length - ok.length,
    rate: done.length ? Math.round((ok.length / done.length) * 100) : null,
    avg: durations.length ? Math.round(durations.reduce((a, b) => a + b, 0) / durations.length) : null,
    p95: percentile(durations, 0.95),
    lastWeek: done.filter((r) => { const at = parseUTC(r.executed_at); return at && at.getTime() >= weekAgo; }).length,
    crashes: runs.filter((r) => r.triggered_by === "AUTO_RESTART").length,
    lastFailure: done.find((r) => r.status === "FAILED") || null,
    days,
  };
}

function runsPerDay(stats) {
  const top = Math.max(1, ...stats.days.map((d) => d.ok + d.failed));
  return h("div", {}, h("div", { class: "chart-h" }, h("b", {}, "Runs per day"), h("span", { class: "dim small" }, "last 14 days")),
    h("div", { class: "bars", style: "height:80px;gap:4px" }, stats.days.map((d) => {
      const n = d.ok + d.failed;
      return h("i", { class: d.failed ? (d.ok ? "mixed" : "FAILED") : "", style: "max-width:none;cursor:default;height:" + (n ? Math.max(8, (n / top) * 100) : 2) + "%;" + (n ? "" : "background:var(--panel3)"),
        title: fmtStamp(d.day).slice(0, 5) + ": " + d.ok + " succeeded, " + d.failed + " failed" });
    })),
    h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--ok)" }), "All succeeded"), h("span", {}, h("i", { style: "background:var(--warn)" }), "Some failed"), h("span", {}, h("i", { style: "background:var(--err)" }), "All failed"),
      h("span", { class: "right" }, fmtStamp(stats.days[0].day).slice(0, 5) + " → today")));
}

function diskBar(disk) {
  if (!disk) return null;
  const usedPct = disk.total ? ((disk.total - disk.free) / disk.total) * 100 : 0;
  return h("div", { class: "box tile" }, h("div", { class: "lbl" }, "Disk " + disk.mount), h("div", { class: "val" }, fmtBytes(disk.free), h("small", {}, " free")),
    h("div", { class: "bar" }, h("i", { style: "width:" + usedPct + "%" + (usedPct > 90 ? ";background:var(--err)" : "") })), h("div", { class: "note" }, usedPct.toFixed(0) + "% of " + fmtBytes(disk.total) + " used"));
}

function metricsBody(id) {
  const body = h("div");
  const switcher = h("div", { class: "termtabs" });
  let range = ui.metricRange || "live";
  let data = null, seq = 0;
  const RANGES = [["live", "Live (15 min)"], ["24h", "24 hours"], ["7d", "7 days"]];
  const paintSwitch = () => switcher.replaceChildren(RANGES.map(([k, label]) => btn(label, () => { range = ui.metricRange = k; data = null; paintSwitch(); paint(); load(); }, "sm" + (k === range ? " on" : ""))));

  function paintLive(t) {
    const [m, hh, runs, pr] = data;
    const last = m.samples[m.samples.length - 1];
    const live = t.running && last;
    const lat = hh.samples.filter((x) => x.latency_ms !== null);
    const stats = deployStats(runs);
    const recent = runs.filter((r) => !r.label).slice(0, 40).reverse();
    const maxDur = Math.max(1, ...recent.map((r) => r.duration_ms || 0));
    const series = (pick) => m.samples.map((x) => ({ t: x.t, v: pick(x) }));
    return [
      !t.running && !m.samples.length ? h("div", { class: "note-box" }, "CPU, memory and disk activity are sampled every 5 seconds while the target is running, across its whole process tree. Start it to see live figures. The 24-hour and 7-day views keep what was recorded earlier.") : null,
      h("div", { class: "grid g-tiles", style: "margin-bottom:12px" },
        tile("CPU", live ? fmtPct(last.cpu) : "-", "of all " + m.cpus + " cores"), tile("Memory", live ? fmtBytes(last.mem) : "-", live ? last.procs + " process(es)" : "not running"),
        tile("Disk writes", live ? fmtRate(last.write_bps) : "-", live ? "reads " + fmtRate(last.read_bps) : ""),
        t.health_url ? tile("Response time", t.health && t.health.latency_ms !== null ? t.health.latency_ms + " ms" : "-", t.uptime_pct !== null ? t.uptime_pct + "% reachable this hour" : "", t.health && t.health.status === "DOWN" ? "err" : null) : null,
        tile("Up for", t.running ? fmtUp(t.running.uptime_secs) : "-", t.running ? "since " + fmtClock(new Date(t.running.started_ms)) : "stopped"), diskBar(pr.disk)),
      h("div", { class: "grid g2" },
        h("div", { class: "box" }, chartBlock("CPU", live ? fmtPct(last.cpu) : "", series((x) => x.cpu), { minTop: 1, fmt: fmtPct, note: "of all " + m.cpus + " cores", empty: "Not running", label: "CPU usage of the process tree" })),
        h("div", { class: "box" }, chartBlock("Memory", live ? fmtBytes(last.mem) : "", series((x) => x.mem), { fmt: fmtBytes, note: "whole process tree", empty: "Not running", label: "Memory of the process tree" })),
        h("div", { class: "box" }, chartBlock("Disk writes", live ? fmtRate(last.write_bps) : "", series((x) => x.write_bps), { minTop: 1024, fmt: fmtRate, note: "bytes written per second", empty: "Not running", label: "Disk writes of the process tree" })),
        h("div", { class: "box" }, chartBlock("Disk reads", live ? fmtRate(last.read_bps) : "", series((x) => x.read_bps), { minTop: 1024, fmt: fmtRate, note: "bytes read per second", empty: "Not running", label: "Disk reads of the process tree" })),
        t.health_url ? h("div", { class: "box" }, chartBlock("Health response time", lat.length ? lat[lat.length - 1].latency_ms + " ms" : "", lat.map((x) => ({ t: x.t, v: x.latency_ms })),
          { fmt: fmtMs, note: "last " + hh.samples.length + " probes", empty: "No successful probes yet", label: "Health probe response time" })) : null,
        h("div", { class: "box" }, h("div", { class: "chart-h" }, h("b", {}, "Run durations"), h("span", { class: "dim small" }, "last " + recent.length + " runs, oldest first")),
          recent.length ? [h("div", { class: "bars" }, recent.map((r) => h("i", { class: r.status, style: "height:" + Math.max(4, ((r.duration_ms || 0) / maxDur) * 100) + "%",
              title: "#" + r.id + " " + r.status.toLowerCase() + " · " + fmtDur(r.duration_ms) + " · " + fmtDT(parseUTC(r.executed_at)), onclick: () => go("target/" + id + "/deployments/" + r.id) }))),
            h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--ok)" }), "Success"), h("span", {}, h("i", { style: "background:var(--err)" }), "Failed"), h("span", {}, h("i", { style: "background:var(--info)" }), "In progress"), h("span", { class: "right" }, "longest " + fmtDur(maxDur)))]
            : h("div", { class: "muted small", style: "padding:20px 0" }, "No runs yet."))),
      h("h3", {}, "Processes", h("span", { class: "dim small", style: "font-weight:400" }, "  everything the script started, largest first")),
      pr.processes.length ? table(["PID", "Process", "CPU", "Memory"], pr.processes.map((p) => [h("span", { class: "mono" }, p.pid), p.name, fmtPct(p.cpu), fmtBytes(p.mem)]))
        : h("div", { class: "muted small" }, t.running ? "Collecting…" : "Not running."),
      h("h3", {}, "Deployments"),
      h("div", { class: "grid g-tiles", style: "margin-bottom:12px" },
        tile("Success rate", stats.rate === null ? "-" : [stats.rate, h("small", {}, " %")], stats.ok + " succeeded, " + stats.failed + " failed", stats.rate !== null && stats.rate < 80 ? "warn" : null),
        tile("Typical duration", fmtDur(stats.avg), stats.p95 !== null ? "95% finish within " + fmtDur(stats.p95) : ""),
        tile("Runs this week", stats.lastWeek, stats.total + " in the stored history"),
        tile("Crash restarts", stats.crashes, stats.crashes ? "automatic restarts after a crash" : "none recorded", stats.crashes ? "warn" : null),
        tile("Last failure", stats.lastFailure ? when(parseUTC(stats.lastFailure.executed_at)) : "none", stats.lastFailure ? "run #" + stats.lastFailure.id : "in the stored history", stats.lastFailure ? "err" : null)),
      h("div", { class: "box" }, runsPerDay(stats)),
    ];
  }

  function paintHistory(t) {
    const d = data[0];
    const o = { stamp: fmtStamp };
    const pts = (pick) => d.points.map((p) => ({ t: p.t, v: pick(p) })).filter((p) => p.v !== null && p.v !== undefined);
    const span = range === "7d" ? "7 days" : "24 hours";
    if (!d.minutes_recorded) return [h("div", { class: "note-box" }, "Nothing has been recorded for the last " + span + " yet. One row per minute is stored while the target is running or has a health URL, and kept for 7 days.")];
    const avail = pts((p) => (p.probes ? (p.probes_up / p.probes) * 100 : null));
    return [
      h("div", { class: "grid g-tiles", style: "margin-bottom:12px" },
        t.health_url || d.availability_pct !== null ? tile("Availability", d.availability_pct === null ? "-" : [d.availability_pct, h("small", {}, " %")], "share of health probes answered", d.availability_pct !== null && d.availability_pct < 99 ? "warn" : null) : null,
        d.latency_avg !== null ? tile("Response time", fmtMs(d.latency_avg), "typical " + fmtMs(d.latency_p50) + " · 95% under " + fmtMs(d.latency_p95)) : null,
        tile("Outages", d.outages.length, d.outages.length ? "longest " + Math.max(...d.outages.map((x) => x.minutes)) + " min" : "none in this period", d.outages.length ? "err" : null),
        tile("Time running", fmtUp(d.running_minutes * 60), "of the last " + span),
        d.cpu_avg !== null ? tile("CPU", fmtPct(d.cpu_avg), "peak " + fmtPct(d.cpu_peak)) : null,
        d.mem_peak !== null ? tile("Peak memory", fmtBytes(d.mem_peak)) : null),
      h("div", { class: "grid g2" },
        h("div", { class: "box" }, chartBlock("CPU", "", pts((p) => p.cpu), Object.assign({ minTop: 1, fmt: fmtPct, note: "average per " + d.bucket_secs / 60 + " min", empty: "Was not running", label: "CPU usage history" }, o))),
        h("div", { class: "box" }, chartBlock("Memory", "", pts((p) => p.mem), Object.assign({ fmt: fmtBytes, note: "average per " + d.bucket_secs / 60 + " min", empty: "Was not running", label: "Memory history" }, o))),
        avail.length ? h("div", { class: "box" }, chartBlock("Availability", "", avail, Object.assign({ max: 100, fmt: (v) => Math.round(v) + "%", note: "health probes answered", label: "Availability history" }, o))) : null,
        avail.length ? h("div", { class: "box" }, chartBlock("Health response time", "", pts((p) => p.latency), Object.assign({ fmt: fmtMs, note: "average per " + d.bucket_secs / 60 + " min", empty: "No successful probes", label: "Response time history" }, o))) : null),
      h("h3", {}, "Outages", h("span", { class: "dim small", style: "font-weight:400" }, "  periods in which every health probe failed")),
      d.outages.length ? table(["From", "Until", "Duration"], d.outages.map((x) => [fmtDT(new Date(x.from * 1000)), fmtDT(new Date(x.to * 1000)), fmtUp(x.minutes * 60)]))
        : h("div", { class: "note-box ok" }, t.health_url ? "No outages in the last " + span + "." : "This target has no health URL, so outages cannot be detected."),
    ];
  }

  const paint = () => {
    const t = T(id);
    if (!data || !t) return body.replaceChildren(h("p", { class: "muted" }, "Loading…"));
    body.replaceChildren(range === "live" ? paintLive(t) : paintHistory(t));
  };
  const load = () => {
    const mine = ++seq, base = "/api/targets/" + id;
    const calls = range === "live" ? [api("GET", base + "/metrics"), api("GET", base + "/health/history"), api("GET", base + "/history"), api("GET", base + "/processes")]
      : [api("GET", base + "/metrics/history?range=" + range)];
    Promise.all(calls).then((d) => { if (mine !== seq) return; data = d; if (!body.querySelector(".chart:hover")) paint(); }, () => {});
  };
  paintSwitch(); paint(); load();
  const timer = setInterval(() => { if (range === "live") load(); }, 5000);
  const slow = setInterval(() => { if (range !== "live") load(); }, 60000);
  return { el: h("div", {}, switcher, body), cleanup: () => { clearInterval(timer); clearInterval(slow); } };
}
