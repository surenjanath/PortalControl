"use strict";
// ---- configuration: network ----
// Who can open this project: only this computer, or other computers too.
// The page always shows what is really reachable right now (PortalControl
// tries to connect), and changes the setting when the start script is one it
// wrote. A script of your own is never edited; the page says what to put in it.

CFG.network = (ctx) => {
  const { t, id } = ctx;
  const box = h("div", {}, h("p", { class: "muted" }, "Checking what is reachable…"));
  let data = null;

  const yesNo = (open) => h("span", { class: "pill " + (open ? "ok" : "off") }, open ? h("span", { class: "dot ok" }) : null, open ? "answering" : "not answering");
  const link = (url) => h("a", { href: url, target: "_blank", rel: "noopener", class: "mono", style: "text-decoration:underline" }, url);

  const paint = () => {
    if (!data.port) {
      return box.replaceChildren(h("div", { class: "note-box" }, "PortalControl does not know which port this project uses. Set its address under ",
        h("a", { href: "#/target/" + id + "/configuration/health", style: "text-decoration:underline" }, "Health Check"), " (for example http://127.0.0.1:8000/) and come back."));
    }
    const anyNetwork = data.network.some((n) => n.open);
    const status = h("div", { class: "box", style: "margin-bottom:18px" }, facts([
      ["Port", data.port],
      ["On this computer", h("span", { class: "rowf" }, link(data.local.url), yesNo(data.local.open))],
      ...(data.network.length ? data.network.map((n) => [n.kind === "tailscale" ? "Tailscale" : n.kind === "other" ? "Network" : "Local network", h("span", { class: "rowf" }, link(n.url), yesNo(n.open), h("span", { class: "dim" }, n.interface))])
        : [["Other computers", "This computer has no local-network or Tailscale address right now."]]),
    ]));
    const meaning = !data.local.open ? "Nothing answers on port " + data.port + ". Start the target, then press Check again."
      : anyNetwork ? "Other computers on the same network can open this project at the addresses marked “answering”, provided this computer's firewall lets them in."
      : "Only this computer can open this project. It listens on 127.0.0.1, which other computers cannot reach.";

    let controls;
    if (data.starter) {
      const who = h("select", {}, h("option", { value: "127.0.0.1" }, "Only this computer"), h("option", { value: "0.0.0.0" }, "Other computers on the network too"));
      who.value = data.written_for_network ? "0.0.0.0" : "127.0.0.1";
      const port = h("input", { type: "number", min: "1", max: "65535", value: data.port, style: "max-width:160px" });
      const err = h("span", { class: "errtxt" });
      const save = btn("Save and rewrite the start script", async () => {
        err.textContent = ""; save.disabled = true;
        try {
          const made = await api("POST", "/api/fs/starter", { dir: t.working_directory, starter: data.starter.id, host: who.value, port: parseInt(port.value, 10) || null });
          await saveTarget(T(id) || t, made.health_url ? { health_url: made.health_url } : {}, made.script + " rewritten");
          await load();
          const x = T(id);
          if (x && x.running && await confirmDlg("Restart " + x.name + " now?", "The change is in the start script. It takes effect when the target is started again.", "Restart")) {
            await act(() => api("POST", "/api/targets/" + id + "/restart"), "Restarting…");
            setTimeout(load, 4000);
          }
        } catch (e) { err.textContent = e.message; }
        save.disabled = false;
      }, "hl");
      controls = [
        field("Who can open this project", who, "PortalControl wrote this target's start script (" + data.script + ", " + data.starter.label + "), so it can change where the project listens. “Other computers” makes it listen on every network address of this computer."),
        field("Port", port, "Other computers use http://ADDRESS-OF-THIS-COMPUTER:PORT/. The Health Check address follows the port."),
        h("div", { class: "formfoot" }, save, err),
      ];
    } else {
      controls = [h("div", { class: "note-box" },
        h("b", {}, "This target runs your own script (" + data.script + "), which PortalControl does not edit. "),
        "To let other computers in, make the program in that script listen on 0.0.0.0 instead of 127.0.0.1, then restart the target:",
        h("ul", { style: "margin:8px 0 0 18px" },
          h("li", {}, "Django: ", h("span", { class: "mono" }, "python manage.py runserver 0.0.0.0:" + data.port)),
          h("li", {}, "Flask, Uvicorn, Gunicorn: ", h("span", { class: "mono" }, "--host 0.0.0.0"), " (Gunicorn: ", h("span", { class: "mono" }, "--bind 0.0.0.0:" + data.port), ")"),
          h("li", {}, "Node: listen on ", h("span", { class: "mono" }, "0.0.0.0"), " (many servers read the ", h("span", { class: "mono" }, "HOST"), " variable; set it under Environment Variables)")),
        h("div", { style: "margin-top:8px" }, "Or pick a start script written by PortalControl under ",
          h("a", { href: "#/target/" + id + "/configuration/general", style: "text-decoration:underline" }, "General"), "; then this page can switch it for you."))];
    }

    box.replaceChildren(status, h("p", { style: "margin:-6px 0 16px" }, meaning, " ", btn("Check again", load, "sm")), controls,
      h("h3", {}, "Good to know"),
      h("ul", { class: "muted small", style: "margin-left:18px" },
        h("li", {}, "Firewall: the first time a program listens for other computers, Windows asks whether to allow it. It must be allowed on private networks, or the addresses above answer here but not from elsewhere."),
        h("li", {}, "Django refuses addresses it does not know: add this computer's address or name to ALLOWED_HOSTS in the project's settings."),
        h("li", {}, "Host name (domain): a name works as soon as your DNS, router or hosts file points it at this computer; the project then answers at http://NAME:" + data.port + "/."),
        h("li", {}, "HTTPS: PortalControl encrypts its own dashboard (Settings › Network) but does not put HTTPS in front of projects yet. A project is served the way its own program serves it.")));
  };

  const load = () => api("GET", "/api/targets/" + id + "/network").then((d) => { data = d; paint(); },
    (e) => box.replaceChildren(h("div", { class: "note-box err" }, e.message)));
  load();
  return [h("div", { class: "note-box" }, "Whether other computers can open this project in their browser. This is separate from who can open the PortalControl dashboard, which is under Settings › Network."), box];
};
