"use strict";
// ---- settings: access ----
// Set, change or remove the dashboard password.
SET.access = () => {
  const set = !!(authInfo && authInfo.password_set);
  const min = (authInfo && authInfo.min_password_chars) || 8;
  const current = h("input", { type: "password", autocomplete: "current-password" });
  const fresh = h("input", { type: "password", autocomplete: "new-password", minlength: String(min) });
  const again = h("input", { type: "password", autocomplete: "new-password" });
  const err = h("span", { class: "errtxt" });
  const send = async (body, done) => {
    err.textContent = "";
    try { await api("POST", "/api/auth/password", body); toast(done); await refresh(true); }
    catch (e) { err.textContent = e.message; }
  };
  const form = h("form", { style: "max-width:520px;margin-top:14px", onsubmit: (e) => {
      e.preventDefault();
      if (fresh.value !== again.value) { err.textContent = "The two new passwords are not the same"; return; }
      if (fresh.value.length < min) { err.textContent = "Use at least " + min + " characters"; return; }
      send({ current: current.value, new: fresh.value }, set ? "Password changed. Other browsers were signed out." : "Password set. This browser stays signed in.");
    } },
    set ? field("Current password", current) : null,
    h("div", { class: "cols" }, field(set ? "New password" : "Password", fresh, "At least " + min + " characters."), field("Repeat it", again)),
    h("div", { class: "formfoot" }, h("button", { class: "btn hl", type: "submit" }, set ? "Change password" : "Set password"),
      set ? btn("Remove password", async () => {
        if (!current.value) { err.textContent = "Enter the current password to remove it"; return; }
        if (await confirmDlg("Remove the password?", "Anyone who can reach this address will be able to use the dashboard again.", "Remove password", true)) send({ current: current.value, new: "" }, "Password removed");
      }, "danger") : null, err));
  return [
    h("div", { class: "box" },
      set ? h("span", { class: "pill ok" }, h("span", { class: "dot ok" }), "Password protected") : h("span", { class: "pill warn" }, h("span", { class: "dot warn" }), "No password"),
      h("div", { class: "muted small", style: "margin-top:8px" }, set
        ? "Sign-in is required for the dashboard and its API. Sessions last 7 days and end when the manager restarts. Deploy webhooks keep working through their own token. Forgotten password: on this machine run portalcontrol stop, then portalcontrol reset-password."
        : "Anyone who can reach " + (sys ? "http://" + sys.bind : "this address") + " can run scripts, commands and database restores. That is fine on 127.0.0.1 for a single-user machine; a password is required before it is opened to other computers under Network."),
      form)];
};
