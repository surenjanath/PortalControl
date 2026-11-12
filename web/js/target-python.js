"use strict";
// ---- configuration: Python environment ----
// Which virtualenv (or conda environment) is switched on for the target's
// runs: none, the one in the project folder, or a folder chosen here.

// One line describing what a setting leads to. `p` is the `python` object of
// a target, or the answer of /api/fs/pyenv.
function pyEnvSummary(p) {
  if (!p) return "-";
  if (p.error) return h("span", { class: "errtxt" }, p.error);
  if (p.found) return h("span", {}, h("span", { class: "mono" }, p.root), p.version ? h("span", { class: "dim" }, "  \u00b7 Python " + p.version) : null);
  return p.mode === "auto" ? "No .venv, venv or env folder with a Python for this system was found in the working directory, so nothing is switched on." : "None. Scripts use whatever python is on PATH.";
}

CFG.python = (ctx) => {
  const { t, live } = ctx;
  const start = (t.python && t.python.mode) || "off";
  const mode = h("select", {},
    h("option", { value: "off" }, "None \u2014 run scripts as they are"),
    h("option", { value: "auto" }, "Automatic \u2014 .venv, venv or env in the working directory"),
    h("option", { value: "custom" }, "A folder I choose"));
  mode.value = start;
  const folder = h("input", { type: "text", spellcheck: "false", value: start === "custom" ? t.python_env || "" : "",
    placeholder: navigator.platform.startsWith("Win") ? "C:\\envs\\myproject  or  ..\\shared-venv" : "/home/me/envs/myproject  or  ../shared-venv" });
  const folderField = field("Environment folder", folder, "The folder that holds Scripts\\python.exe (Windows) or bin/python (macOS, Linux). A conda environment folder works too. A relative path is taken from the working directory.");
  const result = h("div", { class: "hint", style: "margin:-6px 0 14px" });
  const setting = () => (mode.value === "custom" ? folder.value.trim() : mode.value === "auto" ? "auto" : "");
  let seq = 0;
  const check = async () => {
    folderField.style.display = mode.value === "custom" ? "" : "none";
    if (mode.value === "custom" && !folder.value.trim()) return result.replaceChildren("Type the folder of the environment.");
    const mine = ++seq;
    try {
      const r = await api("GET", "/api/fs/pyenv?dir=" + encodeURIComponent(t.working_directory) + "&env=" + encodeURIComponent(setting()));
      if (mine === seq) result.replaceChildren(r.found ? "Found: " : "", pyEnvSummary(r));
    } catch (e) { if (mine === seq) result.replaceChildren(h("span", { class: "errtxt" }, e.message)); }
  };
  mode.onchange = check;
  let timer = null;
  folder.oninput = () => { clearTimeout(timer); timer = setTimeout(check, 350); };
  check();
  return [
    h("div", { class: "note-box" }, "The environment chosen here is switched on for every run of this target, the way an activate script would do it: its programs go to the front of ",
      h("span", { class: "mono" }, "PATH"), " and ", h("span", { class: "mono" }, "VIRTUAL_ENV"), " is set. A plain ", h("span", { class: "mono" }, "python"), " or ", h("span", { class: "mono" }, "pip"),
      " in your script is then the environment's. The commands on the Commands tab use it too. Changes apply to the next run."),
    live((x) => h("div", { class: "box", style: "margin-bottom:22px" }, facts([["In use now", pyEnvSummary(x.python)], x.python && x.python.found ? ["Python", h("span", { class: "mono" }, x.python.python)] : null]))),
    saveForm(t, [field("Python environment", mode), folderField, result], () => ({ python_env: setting() }), "Python environment saved"),
  ];
};
