"use strict";
// ------------------------------------------------------------------- about --
// Which PortalControl this is: version, build, where it runs and where it
// keeps its files. Everything comes from /api/system.

function aboutRows() {
  const host = sys.host || {};
  const built = sys.built_at ? new Date(sys.built_at * 1000) : null;
  return {
    "This build": [
      ["Version", sys.version],
      ["Author", sys.author],
      ["Commit", sys.commit || "not recorded"],
      ["Built", built ? fmtDT(built) : "not recorded"],
      ["Platform", sys.os + " " + sys.arch],
      ["SQLite", sys.sqlite],
      ["libgit2", sys.libgit2],
    ],
    "This machine": [
      ["Computer", host.name],
      ["System", host.os_version],
      ["Processors", host.cpus],
      ["Memory", host.mem_total ? fmtBytes(host.mem_total) : null],
      ["Shell for scripts", sys.shell],
    ],
    "Running now": [
      ["Address", "http://" + sys.bind + "/"],
      ["Started", sys.started_at ? fmtDT(new Date(sys.started_at * 1000)) : null],
      ["Up for", fmtUp(sys.uptime_secs)],
      ["Process id", sys.pid],
      ["Targets", sys.targets],
      ["Start at login", sys.autostart ? "On" : "Off"],
    ],
    "Files": [
      ["Program", sys.executable],
      ["Data folder", sys.data_dir],
      ["Database", sys.data_dir ? sys.data_dir + (sys.os === "windows" ? "\\" : "/") + "portalcontrol.db" : null],
    ],
  };
}

function aboutView() {
  return simple("about", () => {
    if (!sys) return h("p", { class: "muted" }, "Loading…");
    const groups = aboutRows();
    const asText = () => "PortalControl " + sys.version + "\n" + Object.entries(groups).map(([title, rows]) =>
      "\n" + title + "\n" + rows.filter((r) => r[1] !== null && r[1] !== undefined && r[1] !== "").map(([k, v]) => "  " + k + ": " + v).join("\n")).join("\n");
    const mono = new Set(["Commit", "Program", "Data folder", "Database", "Address", "Shell for scripts"]);
    const group = ([title, rows]) => h("div", {}, h("h2", {}, title),
      h("div", { class: "box" }, facts(rows.map(([k, v]) => [k, mono.has(k) && v ? h("span", { class: "mono", style: "word-break:break-all" }, v) : v]))));
    return [
      h("div", { class: "head" },
        h("div", { class: "rowf grow", style: "gap:16px" },
          h("img", { src: "/assets/icon.svg", width: "56", height: "56", alt: "" }),
          h("div", {}, h("h1", {}, "PortalControl"),
            h("p", { class: "sub" }, sys.tagline || ""),
            h("p", { class: "sub" }, "Version " + sys.version, sys.author ? [" · created by ", sys.homepage ? h("a", { href: sys.homepage, target: "_blank", rel: "noopener", style: "text-decoration:underline" }, sys.author) : sys.author] : null))),
        btn("Copy details", () => copyText(asText()), "", "copy", { title: "Copy everything on this page as text" })),
      // Three short groups side by side, then the long file paths at full width.
      h("div", { class: "grid g2" }, Object.entries(groups).filter(([title]) => title !== "Files").map(group)),
      group(["Files", groups.Files]),
      h("p", { class: "muted small", style: "margin-top:18px" }, "A new version replaces only the program file. Targets, history and settings stay in the data folder and are kept."),
    ];
  });
}
