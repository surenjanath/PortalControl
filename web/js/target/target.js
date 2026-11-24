"use strict";
// ------------------------------------------------------------------ target --
const TABS = [["configuration", "Configuration"], ["deployments", "Deployments"], ["commands", "Commands"], ["logs", "Logs"], ["metrics", "Metrics"], ["security", "Security"]];
const SUBS = [["general", "General"], ["processes", "Related Processes"], ["environment", "Environment Variables"], ["python", "Python Environment"], ["network", "Network"], ["git", "Git Source"], ["health", "Health Check"], ["schedule", "Scheduled Task"],
  ["webhooks", "Webhooks"], ["backups", "Database Backups"], ["advanced", "Advanced"], ["danger", "Danger Zone"]];

async function saveTarget(t, patch, msg) {
  const body = Object.assign({ name: t.name, description: t.description, project: t.project, working_directory: t.working_directory, script_name: t.script_name,
    git_branch: t.git_branch, health_url: t.health_url || "", run_mode: t.run_mode, timeout_secs: t.timeout_secs, autostart: t.autostart, env_vars: t.env_vars,
    schedule: t.schedule || "", auto_deploy: t.auto_deploy, auto_restart: t.auto_restart, db_file: t.db_file || "", backup_on_deploy: t.backup_on_deploy, start_order: t.start_order, python_env: t.python_env || "", git_dir: t.git_dir || "" }, patch);
  await api("PUT", "/api/targets/" + t.id, body);
  toast(msg || "Configuration saved");
  await refresh(true);
}
// A form whose Save button reports errors inline.
function saveForm(t, kids, collect, msg) {
  const err = h("span", { class: "errtxt" });
  const save = h("button", { class: "btn hl", type: "submit" }, "Save");
  return h("form", { onsubmit: async (e) => {
    e.preventDefault(); err.textContent = ""; save.disabled = true;
    try { await saveTarget(t, collect(), msg); } catch (ex) { err.textContent = ex.message; save.disabled = false; }
  } }, kids, h("div", { class: "formfoot" }, save, err));
}

function targetView(id, tab, sub) {
  if (tab === "configuration" && !sub) sub = "general";
  const def = { key: "target/" + id + "/" + tab + "/" + (sub || "") };
  def.build = () => {
    const t = T(id);
    if (!t) return loaded ? empty("Target not found", "It may have been deleted.", h("a", { class: "btn", href: "#/projects" }, "Back to projects")) : h("p", { class: "muted" }, "Loading\u2026");
    const head = h("div");
    const paintHead = () => {
      const x = T(id); if (!x) return;
      head.replaceChildren(
        h("div", { class: "crumbs" }, h("a", { href: "#/projects" }, "Projects"), icon("chevron"), h("a", { href: projHref(x.project) }, x.project), icon("chevron"), x.name),
        h("div", { class: "head" },
          h("div", { class: "grow" }, h("div", { class: "rowf" }, h("h1", {}, x.name), pill(tStatus(x)), h("span", { class: "tag" }, x.run_mode === "SERVICE" ? "Service" : "Task"),
              x.command_running ? h("a", { class: "tag info pulse", href: "#/target/" + id + "/deployments/" + x.command_running.run_id }, "command running") : null),
            h("p", { class: "sub" }, x.description || h("span", { class: "mono" }, x.working_directory + " \u279c " + x.script_name))),
          h("div", { class: "rowf" }, x.health_url ? h("a", { class: "btn", href: x.health_url, target: "_blank", rel: "noopener" }, icon("external"), "Open") : null, actionButtons(x))));
    };
    paintHead();
    const body = { configuration: cfgBody, deployments: sub ? runBody : depsBody, commands: commandsBody, logs: logsBody, metrics: metricsBody, security: secBody }[tab];
    const inner = body ? body(id, sub) : { el: empty("Unknown tab", "") };
    def.update = () => { if (!T(id)) return show(true); paintHead(); if (inner.update) inner.update(); };
    def.cleanup = inner.cleanup;
    return h("div", {}, head,
      h("nav", { class: "tabs" }, TABS.map(([k, label]) => h("a", { href: "#/target/" + id + "/" + k, class: k === tab ? "on" : null }, label))),
      inner.el);
  };
  return def;
}

