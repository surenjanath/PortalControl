"use strict";
// ---- settings: start at login, notifications, export and import ----
SET.startup = () => {
  const c = h("div");
  const paint = () => {
    if (!sys) return;
    c.replaceChildren(
        h("div", { class: "box rowf" }, h("div", { style: "flex:1;min-width:240px" }, sys.autostart ? [h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "Enabled"), h("div", { class: "mono dim", style: "margin-top:8px;word-break:break-all" }, sys.autostart)]
            : [h("span", { class: "pill off" }, "Disabled"), h("div", { class: "muted small", style: "margin-top:8px" }, "PortalControl only runs while you start it yourself. Enable this to bring it (and every target marked \u201cStart with PortalControl\u201d) up when you log in.")]),
          sys.autostart ? btn("Disable", () => act(() => api("DELETE", "/api/system/autostart"), "Start at login disabled"), "danger") : btn("Enable start at login", () => act(() => api("POST", "/api/system/autostart"), "Start at login enabled"), "hl")),
    );
  };
  paint();
  return { el: c, update: paint };
};

SET.notifications = () => {
  const c = h("div");
  const paint = () => {
    if (!sys) return;
    c.replaceChildren(
        h("div", { class: "box rowf" }, h("div", { style: "flex:1;min-width:240px" }, notifyOn() ? h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "On in this browser") : h("span", { class: "pill off" }, "Off"),
            h("div", { class: "muted small", style: "margin-top:8px" }, "Desktop notifications when a run fails, a service goes down or crashes, and when a run finishes while this tab is in the background. Needs the dashboard open in a browser tab.")),
          btn(notifyOn() ? "Turn off" : "Turn on notifications", toggleNotify, notifyOn() ? "" : "hl")),
    );
  };
  paint();
  return { el: c, update: paint };
};

SET.transfer = () => {
        const withValues = h("input", { type: "checkbox" });
        return h("div", { class: "box rowf" }, h("div", { class: "muted small", style: "flex:1;min-width:240px" }, "Save every target's configuration (not history, snapshots or webhook tokens) to a JSON file, or load one on another machine. On import, targets that are already registered are skipped and ones whose folder does not exist here are reported.",
            h("label", { class: "rowf", style: "gap:6px;margin-top:8px;color:var(--text)" }, withValues, "Include environment variable values (passwords, API keys). Off: only their names are saved.")),
          btn("Export", async () => {
            try {
              const data = await api("GET", "/api/export" + (withValues.checked ? "?env_values=1" : ""));
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
          }));
};
