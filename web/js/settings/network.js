"use strict";
// ---- settings: network ----
// Who can open the dashboard: only this computer, or other computers too, on
// which address and port, under which host names, and with or without HTTPS.
// The settings are always shown. They are read when PortalControl starts, so
// a change is saved at once but used only after a restart; the section says
// so and offers the restart.

SET.network = () => {
  const status = h("div"), form = h("div", {}, h("p", { class: "muted" }, "Loading\u2026"));
  let data = null;

  const link = (url) => h("a", { href: url, class: "mono", style: "text-decoration:underline" }, url);
  // The limits that apply to other computers right now.
  const limitNote = () => {
    const l = data.limits;
    if (l.expired) return h("span", { class: "pill err", style: "margin-left:8px" }, "closed: the time ran out");
    return h("span", { class: "dim" }, (l.expires_at ? "  \u00b7 closes at " + fmtDT(new Date(l.expires_at * 1000)) : "") + (l.allow_other_networks ? "  \u00b7 from any network" : data.subnet ? "  \u00b7 only from " + data.subnet : ""));
  };
  const paintStatus = () => {
    const a = data.active;
    status.replaceChildren(
      h("div", { class: "box", style: "margin-bottom:14px" }, facts([
        ["On this computer", h("span", {}, link(a.local_url), h("span", { class: "dim" }, "  always on"))],
        ["Other computers", a.network_url ? h("span", {}, link(a.network_url), h("span", { class: "dim" }, a.https ? "  encrypted (HTTPS)" : "  not encrypted"), limitNote()) : "Off. Only this computer can open the dashboard."],
        a.hosts.length ? ["Host names", h("span", { class: "mono" }, a.hosts.join(", "))] : null,
      ])),
      a.network_url ? h("div", { style: "margin-bottom:14px" }, h("h3", { style: "margin-top:0" }, "Who has connected"),
        data.clients.length ? table(["Computer", "Last seen", "Requests", ""], data.clients.map((c) => [h("span", { class: "mono" }, c.ip), when(new Date(c.last_seen * 1000)), c.requests,
          c.refused ? h("span", { class: "pill err", title: c.refused }, "refused") : h("span", { class: "pill ok" }, "allowed")]))
          : h("p", { class: "muted small" }, "No other computer has connected since PortalControl started. Sign-ins and refusals also appear under Activity.")) : null,
      data.restart_needed ? h("div", { class: "note-box warn rowf" },
        h("div", { style: "flex:1;min-width:240px" }, h("b", {}, "Saved, but not in use yet. "), "Network settings are read when PortalControl starts. What you see above is what is running now; the settings below take effect after a restart."),
        btn("Restart now", restart, "hl", "restart")) : null,
      data.bind_from_cli ? h("div", { class: "note-box" }, "PortalControl was started with ", h("span", { class: "mono" }, "--bind"), ", which takes the place of the address chosen below for as long as it is started that way.") : null,
      data.port_from_cli ? h("div", { class: "note-box" }, "PortalControl was started with ", h("span", { class: "mono" }, "--port"), ", which takes the place of the port below for as long as it is started that way.") : null,
      data.warnings.map((w) => h("div", { class: "note-box warn" }, w)));
  };

  async function restart() {
    if (!(await confirmDlg("Restart PortalControl?", "Every running target is stopped. Targets set to start with PortalControl come back by themselves; the others have to be started by hand.", "Restart", true))) return;
    const s = data.saved, port = s.port || data.default_port;
    const local = ["127.0.0.1", "localhost"].includes(location.hostname);
    // Where this page will be after the restart: the port, and for other
    // computers the scheme, may have changed.
    const next = (local ? "http" : s.https ? "https" : "http") + "://" + location.hostname + ":" + port + "/#/settings/network";
    try { await api("POST", "/api/system/restart"); } catch (e) { return toast(e.message, true); }
    toast("Restarting\u2026 this page reloads in a few seconds");
    setTimeout(() => { location.href = next; location.reload(); }, 6000);
  }

  const buildForm = () => {
    const s = data.saved;
    const known = data.addresses.some((a) => a.ip === s.address);
    const who = h("select", {},
      h("option", { value: "" }, "Only this computer"),
      data.addresses.map((a) => h("option", { value: a.ip }, "Other computers too, through " + a.ip + "  (" + (a.kind === "tailscale" ? "Tailscale" : a.kind === "other" ? "not a private range" : "local network") + ", " + a.interface + ")")),
      s.address && !known ? h("option", { value: s.address }, "Other computers too, through " + s.address + "  (not available right now)") : null,
      h("option", { value: "other" }, "Another address of this computer\u2026"));
    who.value = s.address;
    const other = h("input", { type: "text", spellcheck: "false", placeholder: "192.168.1.20" });
    const otherField = field("Address", other, "An address this computer has on your network. Normally 10.x, 172.16-31.x, 192.168.x or a Tailscale 100.x address.");
    // Some networks use addresses outside the private ranges internally.
    // PortalControl accepts one only when it is told the network is internal.
    const isPrivate = (ip) => { const o = ip.split(".").map(Number); return o[0] === 10 || (o[0] === 172 && o[1] >= 16 && o[1] <= 31) || (o[0] === 192 && o[1] === 168) || (o[0] === 100 && o[1] >= 64 && o[1] <= 127); };
    const chosen = () => (who.value === "other" ? other.value.trim() : who.value);
    const internal = h("input", { type: "checkbox", checked: s.internal_confirmed });
    const internalField = h("label", { class: "check", style: "margin:4px 0 14px" }, internal, h("span", {}, h("b", {}, "This address is on our internal network, not on the internet"),
      h("div", { class: "muted small" }, "It is outside the ranges reserved for private networks, so PortalControl cannot tell by itself. Tick this only if a firewall or router keeps this computer unreachable from the internet. PortalControl runs scripts and must never face the internet.")));
    const port = h("input", { type: "number", min: "1", max: "65535", value: s.port || "", placeholder: String(data.default_port), style: "max-width:160px" });
    const hosts = h("input", { type: "text", spellcheck: "false", value: s.hosts.join(", "), placeholder: "portal.mycompany.lan, " + (data.computer_name || "my-pc").toLowerCase() });
    const https = h("input", { type: "checkbox", checked: s.https });
    const cert = h("input", { type: "text", spellcheck: "false", value: s.cert_file, placeholder: navigator.platform.startsWith("Win") ? "C:\\certs\\portal.pem" : "/etc/ssl/portal.pem" });
    const key = h("input", { type: "text", spellcheck: "false", value: s.key_file, placeholder: navigator.platform.startsWith("Win") ? "C:\\certs\\portal.key" : "/etc/ssl/portal.key" });
    const certFields = h("div", {},
      field("Certificate file", cert, "Full path of a PEM file on this computer that starts with -----BEGIN CERTIFICATE-----. Put the chain in the same file if there is one."),
      field("Private key file", key, "Full path of the matching PEM key file (-----BEGIN PRIVATE KEY-----). It stays on this computer; only the path is stored."));
    const err = h("span", { class: "errtxt" });
    const save = h("button", { class: "btn hl", type: "submit" }, "Save");
    const sameNet = h("input", { type: "checkbox", checked: !s.allow_other_networks });
    const timed = !!s.expires_at && !data.limits.expired;
    const keep = h("select", {}, timed ? h("option", { value: "same" }, "Leave as it is (closes at " + fmtDT(new Date(s.expires_at * 1000)) + ")") : null,
      h("option", { value: "0" }, "Until I switch it off"), h("option", { value: "15" }, "For 15 minutes, then close"),
      h("option", { value: "60" }, "For 1 hour, then close"), h("option", { value: "480" }, "For 8 hours, then close"));
    keep.value = timed ? "same" : "0";
    const limitFields = h("div", {},
      h("label", { class: "check", style: "margin:4px 0 14px" }, sameNet, h("span", {}, h("b", {}, "Only computers on the same network as that address" + (data.subnet ? " (" + data.subnet + ")" : "")),
        h("div", { class: "muted small" }, "Computers from any other network are refused, even where a router would let them through. Untick only if people on another of your networks must reach it."))),
      field("Keep it open", keep, "A time limit suits a quick setup from another computer: when it runs out, other computers are refused until you open it again here. These two limits apply at once, without a restart."));
    const sync = () => {
      limitFields.style.display = who.value === "" ? "none" : "";
      otherField.style.display = who.value === "other" ? "" : "none"; certFields.style.display = https.checked ? "" : "none";
      const ip = chosen();
      internalField.style.display = /^[0-9]+([.][0-9]+){3}$/.test(ip) && !isPrivate(ip) ? "" : "none";
    };
    who.onchange = sync; https.onchange = sync; other.oninput = sync; sync();
    const addName = () => {
      const name = (data.computer_name || "").toLowerCase();
      const list = hosts.value.split(",").map((x) => x.trim()).filter(Boolean);
      if (name && !list.includes(name)) { list.push(name); hosts.value = list.join(", "); }
    };

    form.replaceChildren(h("form", { onsubmit: async (e) => {
      e.preventDefault(); err.textContent = ""; save.disabled = true;
      try {
        data = await api("PUT", "/api/system/network", {
          allow_other_networks: !sameNet.checked, expires_at: keep.value === "same" ? s.expires_at : null,
          open_for_minutes: keep.value !== "same" && keep.value !== "0" ? parseInt(keep.value, 10) : null,
          address: chosen(), internal_confirmed: internalField.style.display !== "none" && internal.checked,
          port: port.value.trim() ? parseInt(port.value, 10) : null,
          hosts: hosts.value.split(/[,\s]+/).map((x) => x.trim()).filter(Boolean),
          https: https.checked, cert_file: cert.value.trim(), key_file: key.value.trim() });
        paintStatus(); buildForm();
        toast(data.restart_needed ? "Network settings saved. Restart PortalControl to use them." : "Network settings saved");
      } catch (ex) { err.textContent = ex.message; }
      save.disabled = false;
    } },
      !data.password_set ? h("div", { class: "note-box" }, "The dashboard has no password yet. Set one under Access before opening it to other computers; PortalControl runs scripts, so it refuses to be reachable from the network without one.") : null,
      field("Who can open the dashboard", who, "This computer can always open it at " + data.active.local_url + ". To let other computers in, pick the address of this computer on the network they share with it. They then open http" + (https.checked ? "s" : "") + "://THAT-ADDRESS:PORT/."),
      otherField, internalField, limitFields,
      field("Port", port, "Leave empty for " + data.default_port + ". Changing it also moves the address on this computer."),
      field("Host names (domain)", hosts, h("span", {}, "Names people may type instead of the IP address, separated by commas: a domain such as portal.mycompany.lan, or this computer's name. The name must already lead to this computer (your DNS server, router or hosts file does that); here you only allow PortalControl to answer to it. ",
        data.computer_name ? btn("Add this computer's name (" + data.computer_name.toLowerCase() + ")", addName, "sm") : null)),
      h("label", { class: "check", style: "margin:4px 0 14px" }, https, h("span", {}, h("b", {}, "Encrypt connections from other computers (HTTPS)"),
        h("div", { class: "muted small" }, "Needs a certificate made for the address or host name people type. With a self-made certificate, browsers warn until it is trusted on each computer. This computer keeps using plain http://127.0.0.1, which never leaves it."))),
      certFields,
      h("div", { class: "formfoot" }, save, err)));
  };

  api("GET", "/api/system/network").then((d) => { data = d; paintStatus(); buildForm(); },
    (e) => form.replaceChildren(h("div", { class: "note-box err" }, "The network settings could not be loaded: " + e.message)));
  // Keep the status and the list of visitors fresh; the form is left alone.
  const timer = setInterval(() => api("GET", "/api/system/network").then((d) => { if (data) { data = d; paintStatus(); } }, () => {}), 5000);
  return { cleanup: () => clearInterval(timer), el: [
    h("p", { class: "muted small", style: "margin-top:-6px" }, "Who can open this dashboard, and how. To make one of your projects reachable from other computers, use the Network section of that target instead."),
    status, form] };
};
