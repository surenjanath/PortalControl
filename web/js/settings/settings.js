"use strict";
// ---------------------------------------------------------------- settings --
// The Settings page: a side menu and one section at a time, like a target's
// Configuration tab. Each section is SET.<name>(). It returns what to show,
// or { el, update, cleanup } when it follows live data or runs a timer.
// The sections live in the other files of this folder.
const SET = {};
const SETTINGS_SUBS = [["access", "Access"], ["network", "Network"], ["startup", "Start at Login"], ["notifications", "Notifications"], ["alerts", "Email Alerts"], ["monitor", "AI Monitor"], ["transfer", "Export and Import"],
  ["manager", "Manager"], ["resources", "Resources"], ["tools", "Scanner Tools"], ["log", "Manager Log"], ["danger", "Danger Zone"]];

function settingsView(sub) {
  sub = sub || "access";
  const def = { key: "settings/" + sub };
  def.build = () => {
    const made = SET[sub] ? SET[sub]() : empty("Unknown section", "This address does not match a settings section.");
    const part = Array.isArray(made) || made instanceof Node ? { el: made } : made;
    // Sections are built once so typing in a form survives background
    // refreshes; the page is rebuilt only when the password state changes.
    const builtWithPassword = !!(authInfo && authInfo.password_set);
    def.update = () => { if (!!(authInfo && authInfo.password_set) !== builtWithPassword) return show(true); if (part.update) part.update(); };
    def.cleanup = part.cleanup;
    return h("div", {},
      h("div", { class: "head" }, h("div", { class: "grow" }, h("h1", {}, "Settings"), h("p", { class: "sub" }, "PortalControl itself: who can open it, how it starts and how it is doing."))),
      h("div", { class: "cfg" },
        h("nav", { class: "subnav" }, SETTINGS_SUBS.map(([k, label]) => h("a", { href: "#/settings/" + k, class: (k === sub ? "on" : "") + (k === "danger" ? " dz" : "") }, label))),
        h("div", { style: "min-width:0" }, h("h2", { style: "margin-top:0" }, (SETTINGS_SUBS.find((x) => x[0] === sub) || ["", "Settings"])[1]), part.el)));
  };
  return def;
}
