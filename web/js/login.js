"use strict";
// Sign-in page: posts the password and reloads into the dashboard on success.
try { const saved = localStorage.getItem("pc-theme"); if (saved) document.documentElement.dataset.theme = saved; } catch (_) {}

const form = document.getElementById("login");
const field = document.getElementById("password");
const error = document.getElementById("error");
const button = document.getElementById("submit");
const reveal = document.getElementById("reveal");
const caps = document.getElementById("caps");

// Say which PortalControl this is: several machines may run one.
document.getElementById("where").textContent = "Sign in to PortalControl on " + location.hostname + ".";

reveal.addEventListener("click", () => {
  const show = field.type === "password";
  field.type = show ? "text" : "password";
  reveal.textContent = show ? "Hide" : "Show";
  reveal.setAttribute("aria-pressed", String(show));
  field.focus();
});
field.addEventListener("keyup", (e) => { if (e.getModifierState) caps.hidden = !e.getModifierState("CapsLock"); });
field.addEventListener("input", () => { error.textContent = ""; });

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (!field.value) { error.textContent = "Type the password."; field.focus(); return; }
  error.textContent = "";
  button.disabled = true;
  button.textContent = "Signing in\u2026";
  try {
    const res = await fetch("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password: field.value }),
    });
    if (res.ok) { location.reload(); return; }
    // A refusal from the network guard is plain text, the rest is JSON.
    const text = await res.text();
    let message = text;
    try { message = JSON.parse(text).error || text; } catch (_) {}
    error.textContent = message ? message.charAt(0).toUpperCase() + message.slice(1) : "Sign-in failed";
    field.select();
  } catch (_) {
    error.textContent = "PortalControl is not reachable.";
  }
  button.disabled = false;
  button.textContent = "Sign in";
});
