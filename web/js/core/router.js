"use strict";
// ------------------------------------------------------------------ router --
const view = { key: null, update: null, cleanup: null };
function routeParts() { return location.hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent); }
function show(force) {
  const r = routeParts();
  const def = resolve(r);
  if (!force && view.key === def.key && view.update) view.update();
  else {
    const same = view.key === def.key;
    if (view.cleanup) view.cleanup();
    view.key = def.key; view.update = null; view.cleanup = null;
    $("main").replaceChildren(h("div", { class: "page" }, def.build()));
    view.update = def.update || null; view.cleanup = def.cleanup || null;
    if (!same) $("main").scrollTop = 0;
  }
  renderSide(r);
}
// A view whose whole body is repainted on every data refresh.
function simple(key, paint) {
  const def = { key };
  def.build = () => { const c = h("div"); def.update = () => c.replaceChildren(...[paint()].flat()); def.update(); return c; };
  return def;
}
function resolve(r) {
  const [a, b, c, d] = r;
  if (!a) return dashboardView();
  if (a === "projects") return projectsView();
  if (a === "project" && b) return projectView(b);
  if (a === "new") return newView(b);
  if (a === "deployments") return deploymentsView();
  if (a === "security") return securityView();
  if (a === "activity") return activityView();
  if (a === "incidents") return incidentsView();
  if (a === "settings") return settingsView(b);
  if (a === "about") return aboutView();
  if (a === "target" && b) return targetView(Number(b), c || "configuration", d);
  return simple("404", () => empty("Page not found", "This address does not match a page.", h("a", { class: "btn", href: "#/" }, "Back to dashboard")));
}

function renderSide(r) {
  const a = r[0] || "";
  const item = (path, label, ic, on, count) => h("a", { href: "#/" + path, class: on ? "on" : null, title: label }, icon(ic), h("span", { class: "lb" }, label),
    count !== undefined && count !== null ? h("span", { class: "count lb" }, count) : null);
  const failing = targets.filter((t) => tStatus(t).cls === "err").length;
  $("side").replaceChildren(
    h("div", { class: "brand" }, "Portal", h("span", {}, "Control")),
    h("div", { class: "brand-sub" }, sys ? "v" + sys.version + " \u00b7 " + ((sys.host && sys.host.name) || sys.os) : "\u00a0"),
    h("nav", { class: "nav" },
      item("", "Dashboard", "home", a === ""),
      item("projects", "Projects", "box", a === "projects" || a === "project" || a === "new" || a === "target", targets.length || null),
      item("deployments", "Deployments", "list", a === "deployments", sys && sys.active_runs ? sys.active_runs + " active" : null),
      item("security", "Security", "shield", a === "security"),
      item("incidents", "Incidents", "bell", a === "incidents", sys && sys.open_incidents ? sys.open_incidents + " open" : null),
      item("activity", "Activity", "activity", a === "activity", failing ? failing + " failing" : null),
      item("settings", "Settings", "settings", a === "settings"),
      item("about", "About", "info", a === "about")),
    h("button", { class: "btn ghost sm", type: "button", style: "margin:8px 4px 0;justify-content:flex-start;color:var(--dim)", onclick: openPalette, title: "Search and jump (Ctrl+K)" }, "Search…", h("span", { class: "right mono", style: "font-size:11px" }, "Ctrl K")),
    h("div", { class: "side-h" }, "Targets", h("a", { href: "#/new", title: "New target" }, icon("plus"))),
    h("div", { class: "side-list" }, byProject(targets).map(([project, list]) => [
      h("a", { class: "side-p", href: projHref(project), title: "Project " + project + ": all processes and terminals" }, project),
      list.map((t) => {
        const st = tStatus(t);
        return h("a", { class: "side-t" + (a === "target" && Number(r[1]) === t.id ? " on" : ""), href: "#/target/" + t.id, title: t.name + " \u2014 " + st.text },
          h("span", { class: "dot " + (st.cls === "off" ? "" : st.cls) + (st.pulse ? " pulse" : "") }), h("span", { class: "nm" }, t.name));
      })])),
    h("div", { class: "side-foot" }, h("span", { class: "dot " + (wsUp ? "ok" : "err pulse") }), h("span", {}, wsUp ? "Live" : "Reconnecting\u2026"),
      authInfo && authInfo.password_set ? btn("Sign out", async () => { try { await api("POST", "/api/auth/logout"); } catch (_) {} location.reload(); }, "ghost sm", null, { title: "Sign out of the dashboard" }) : null,
      btn("", toggleTheme, "ghost sm", "moon", { title: "Switch light / dark theme", "aria-label": "Switch theme" })));
}
function toggleTheme() {
  const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("pc-theme", next); } catch (_) {}
}
try { const saved = localStorage.getItem("pc-theme"); if (saved) document.documentElement.dataset.theme = saved; } catch (_) {}

