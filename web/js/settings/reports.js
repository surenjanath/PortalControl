"use strict";
// ------------------------------------------------------ settings: reports --
// How the PDF reports look (report.rs): the organisation, its address, a logo
// and the name on the signature line; and the three reports to download.

// Any picture the browser can show, as a JPEG on white, at most 900 x 400 px:
// the PDF writer embeds JPEGs as they are, so no image library is needed.
// The picture is read as a data: URL: the dashboard's Content-Security-Policy
// allows images from 'self' and data: only, so a blob: URL would be refused.
function logoToJpeg(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("The file could not be read"));
    reader.onload = () => drawLogo(reader.result, resolve, reject);
    reader.readAsDataURL(file);
  });
}
function drawLogo(url, resolve, reject) {
  const img = new Image();
  img.onload = () => {
    const nw = img.naturalWidth || 600, nh = img.naturalHeight || 200;
    const scale = Math.min(1, 900 / nw, 400 / nh);
    const w = Math.max(1, Math.round(nw * scale)), ht = Math.max(1, Math.round(nh * scale));
    const c = document.createElement("canvas");
    c.width = w; c.height = ht;
    const g = c.getContext("2d");
    g.fillStyle = "#ffffff"; g.fillRect(0, 0, w, ht);
    g.drawImage(img, 0, 0, w, ht);
    c.toBlob((b) => (b ? resolve(b) : reject(new Error("The picture could not be converted"))), "image/jpeg", 0.92);
  };
  img.onerror = () => reject(new Error("This file is not a picture the browser can read"));
  img.src = url;
}

SET.reports = () => {
  const input = (max, ph) => h("input", { type: "text", maxlength: String(max), placeholder: ph || "" });
  const f = {
    org_name: input(100, "InsightFusionTech"),
    address: input(160, "Street, City, Country"),
    contact: input(160, "Phone · email · website"),
    prepared_by: input(80, "Name printed on the signature line"),
    signatory_title: input(80, "IT Operations Manager"),
    note: h("textarea", { rows: "2", maxlength: "400", placeholder: "Optional: a sentence added to every report's sign-off, for example a confidentiality note." }),
  };
  const logoBox = h("div", { class: "logo-prev" });
  const logoNote = h("div", { class: "hint" });
  const pick = h("button", { class: "btn", type: "button" }, icon("upload"), "Choose logo…");
  let hasLogo = false;
  const paintLogo = () => {
    pick.lastChild.textContent = hasLogo ? "Replace logo…" : "Choose logo…";
    logoBox.replaceChildren(hasLogo ? h("img", { src: "/api/settings/reports/logo?v=" + Date.now(), alt: "Report logo" })
      : h("span", { class: "muted small" }, "No logo yet: the reports show the organisation name in its place."));
  };
  api("GET", "/api/settings/reports").then((r) => {
    Object.keys(f).forEach((k) => { f[k].value = (r.brand && r.brand[k]) || ""; });
    hasLogo = !!r.logo; paintLogo();
  }, (e) => toast(e.message, true));

  const file = h("input", { type: "file", accept: "image/png,image/jpeg,image/svg+xml,image/webp,image/gif", style: "display:none" });
  pick.onclick = () => file.click();
  file.onchange = async () => {
    const chosen = file.files && file.files[0];
    file.value = "";
    if (!chosen) return;
    try {
      const jpeg = await logoToJpeg(chosen);
      const res = await fetch("/api/settings/reports/logo", { method: "PUT", headers: { "Content-Type": "image/jpeg" }, body: jpeg });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(data.error || res.status + " " + res.statusText);
      hasLogo = true; paintLogo();
      logoNote.textContent = "Saved: " + data.width + " × " + data.height + " pixels.";
      toast("Logo saved");
    } catch (e) { toast(e.message, true); }
  };
  const removeLogo = async () => {
    if (!hasLogo) return;
    if (!(await confirmDlg("Remove the logo?", [h("p", {}, "The reports then show the organisation name in its place.")], "Remove", true))) return;
    act(async () => { await api("DELETE", "/api/settings/reports/logo"); hasLogo = false; paintLogo(); logoNote.textContent = ""; }, "Logo removed");
  };

  const err = h("span", { class: "errtxt" });
  const save = h("button", { class: "btn hl", type: "submit" }, "Save");
  const form = h("form", { onsubmit: async (e) => {
    e.preventDefault(); err.textContent = ""; save.disabled = true;
    try {
      const body = {};
      Object.keys(f).forEach((k) => { body[k] = f[k].value; });
      await api("PUT", "/api/settings/reports", body);
      toast("Report details saved");
    } catch (ex) { err.textContent = ex.message; }
    save.disabled = false;
  } },
    h("div", { class: "grid g2", style: "gap:0 18px" },
      field("Organisation", f.org_name, "Shown at the top of every report and in the sign-off. Left empty, the reports say InsightFusionTech."),
      field("Address", f.address, "One line under the logo."),
      field("Contact", f.contact, "Phone, email or website, one line."),
      field("Prepared by", f.prepared_by, "Printed above the signature line. Leave it empty for a blank line to sign."),
      field("Title", f.signatory_title, "Printed under the signature line.")),
    field("Note", f.note),
    h("div", { class: "formfoot" }, save, err));

  const report = (href, title, text) => h("div", { class: "box rep-card" },
    h("b", {}, title), h("div", { class: "muted small", style: "margin:4px 0 10px" }, text),
    h("a", { class: "btn sm", href, download: "" }, icon("download"), "Download PDF"));

  return [
    h("div", { class: "note-box" }, "The PDF reports are laid out like a certificate: your logo and address at the top, a report number and seal code, the verdict, the figures that matter, and a sign-off. They never contain environment variable values, tokens or passwords."),
    h("h3", {}, "Logo"),
    h("div", { class: "box logo-row" }, logoBox,
      h("div", {}, h("div", { class: "rowf" }, pick, btn("Remove", removeLogo, "ghost")),
        h("div", { class: "hint" }, "PNG, JPEG, SVG or WebP. It is converted to a JPEG on a white background and shrunk to at most 900 × 400 pixels. A wide logo works best."),
        logoNote, file)),
    h("h3", {}, "Organisation and signature"),
    form,
    h("h3", {}, "Download"),
    h("div", { class: "grid g3" },
      report("/api/reports/overall.pdf", "Status report", "Every site and application: verdict, figures, items needing attention, 7 days of deployments, notable events."),
      report("/api/reports/security.pdf", "Security assessment", "Score and grade, priority actions, PortalControl's own settings, attacks of the last 30 days, code issues and CVEs per application."),
      h("div", { class: "box rep-card" }, h("b", {}, "Site reports"), h("div", { class: "muted small", style: "margin:4px 0 10px" }, "One per project, with a section per application. Download them from the dashboard, next to each project's name."),
        h("a", { class: "btn sm", href: "#/dashboard" }, "Dashboard"))),
  ];
};
