"use strict";
// Sign-in page: posts the password and reloads into the dashboard on success.
try { const saved = localStorage.getItem("pc-theme"); if (saved) document.documentElement.dataset.theme = saved; } catch (_) {}

document.getElementById("login").addEventListener("submit", async (e) => {
  e.preventDefault();
  const error = document.getElementById("error");
  const button = document.getElementById("submit");
  const field = document.getElementById("password");
  error.textContent = "";
  button.disabled = true;
  try {
    const res = await fetch("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password: field.value }),
    });
    if (res.ok) { location.reload(); return; }
    const data = await res.json().catch(() => ({}));
    error.textContent = data.error || "Sign-in failed";
    field.select();
  } catch (_) {
    error.textContent = "PortalControl is not reachable";
  }
  button.disabled = false;
});
