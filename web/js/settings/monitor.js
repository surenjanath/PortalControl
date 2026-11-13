"use strict";
// ---- settings: email alerts and the AI monitor (alerts.rs, monitor.rs) ----

SET.alerts = () => {
  const c = h("div", { class: "muted" }, "Loading…");
  const text = (attrs) => h("input", Object.assign({ type: "text", spellcheck: "false", autocomplete: "off" }, attrs));
  api("GET", "/api/settings/alerts").then((s) => {
    const f = {
      enabled: h("input", { type: "checkbox", checked: s.enabled }),
      host: text({ value: s.smtp.host, placeholder: "smtp.gmail.com" }),
      port: h("input", { type: "number", min: "1", max: "65535", value: s.smtp.port || 587, style: "max-width:120px" }),
      security: h("select", {}, [["starttls", "STARTTLS (port 587)"], ["tls", "TLS (port 465)"], ["none", "None (local relay only)"]].map(([v, l]) => h("option", { value: v, selected: s.smtp.security === v }, l))),
      username: text({ value: s.smtp.username, placeholder: "you@gmail.com" }),
      password: h("input", { type: "password", autocomplete: "new-password", placeholder: s.password_set ? "saved — leave empty to keep it" : "password or app password" }),
      from: text({ value: s.smtp.from, placeholder: "portalcontrol@yourdomain.com" }),
      to: text({ value: (s.smtp.to || []).join(", "), placeholder: "you@example.com, team@example.com" }),
      min: h("select", {}, [["critical", "Critical only"], ["warning", "Warning and critical"], ["info", "Everything"]].map(([v, l]) => h("option", { value: v, selected: s.min_severity === v }, l))),
    };
    f.security.onchange = () => { f.port.value = f.security.value === "tls" ? 465 : f.security.value === "none" ? 25 : 587; };
    const body = () => ({ enabled: f.enabled.checked, min_severity: f.min.value,
      smtp: { host: f.host.value.trim(), port: Number(f.port.value), security: f.security.value, username: f.username.value.trim(), password: f.password.value,
        from: f.from.value.trim(), to: f.to.value.split(/[,;\s]+/).filter(Boolean) } });
    const result = h("div", { class: "small", style: "min-height:18px;margin-top:8px" });
    const testBtn = btn("Send test email", async () => {
      testBtn.disabled = true; result.className = "small muted"; result.textContent = "Sending…";
      try { const r = await api("POST", "/api/alerts/test", body()); result.className = "small"; result.replaceChildren(h("span", { class: "dot ok" }), " Sent to " + r.to.join(", ") + ". Check the inbox (and the spam folder)."); }
      catch (e) { result.className = "small t-err"; result.textContent = e.message; }
      testBtn.disabled = false;
    }, "", "external");
    c.className = "";
    c.replaceChildren(
      h("div", { class: "note-box" }, "PortalControl emails you when the monitor opens an incident (a failure, a crash, a burst of errors, a new kind of error) with the AI model's explanation and suggested fix. The same problem is emailed at most once every 30 minutes, and at most 12 emails go out per hour. Gmail: turn on 2-step verification and use an app password (myaccount.google.com/apppasswords)."),
      h("div", { class: "box" },
        h("label", { class: "rowf", style: "gap:8px;margin-bottom:14px" }, f.enabled, h("b", {}, "Send email alerts")),
        h("div", { class: "grid g2", style: "gap:10px" }, field("SMTP server", f.host), h("div", { class: "rowf", style: "gap:10px;align-items:flex-end" }, field("Port", f.port), field("Security", f.security))),
        h("div", { class: "grid g2", style: "gap:10px" }, field("User name", f.username, "Usually the full email address. Empty for a relay without sign-in."), field("Password", f.password, "Stored encrypted.")),
        h("div", { class: "grid g2", style: "gap:10px" }, field("From", f.from), field("To", f.to, "One or more addresses, separated by commas.")),
        field("Email when severity is", f.min),
        h("div", { class: "rowf", style: "margin-top:6px" },
          btn("Save", () => act(async () => { await api("PUT", "/api/settings/alerts", body()); f.password.value = ""; f.password.placeholder = "saved — leave empty to keep it"; }, "Alert settings saved"), "hl"),
          testBtn),
        result));
  }, (e) => c.replaceChildren(h("div", { class: "note-box err" }, e.message)));
  return c;
};

SET.monitor = () => {
  const c = h("div", { class: "muted" }, "Loading…");
  const flow = monitorFlow();
  api("GET", "/api/settings/monitor").then((s) => {
    const f = {
      enabled: h("input", { type: "checkbox", checked: s.enabled }),
      ai: h("input", { type: "checkbox", checked: s.ai_enabled }),
      url: h("input", { type: "text", value: s.ollama_url, spellcheck: "false" }),
      model: h("select", {}, s.model ? h("option", { value: s.model }, s.model) : h("option", { value: "" }, "—")),
      logs: h("input", { type: "checkbox", checked: s.email_logs }),
    };
    const modelNote = h("div", { class: "hint" });
    const loadModels = async () => {
      modelNote.className = "hint"; modelNote.textContent = "Asking Ollama for its models…";
      try {
        const r = await api("GET", "/api/ai/models?url=" + encodeURIComponent(f.url.value.trim()));
        const chat = r.models.filter((m) => !/embed/i.test(m.name));
        const keep = f.model.value || s.model;
        f.model.replaceChildren(...chat.map((m) => h("option", { value: m.name, selected: m.name === keep }, m.name + (m.parameters ? " · " + m.parameters : ""))));
        modelNote.textContent = chat.length ? chat.length + " model(s) installed. Small instruct models (3–4B) answer in a few seconds; 7–14B are more precise. Use “Test the model” to compare." : "No models installed. Run for example: ollama pull qwen2.5:3b";
      } catch (e) { modelNote.className = "hint t-err"; modelNote.textContent = e.message; }
    };
    const result = h("div", { style: "margin-top:10px" });
    const testBtn = btn("Test the model", async () => {
      testBtn.disabled = true; result.replaceChildren(h("div", { class: "muted small" }, h("span", { class: "dot info pulse" }), " Asking " + f.model.value + " about a sample incident (a Django checkout failing with “no such column”)…"));
      try {
        const r = await api("POST", "/api/ai/test", { url: f.url.value.trim(), model: f.model.value });
        const a = r.answer;
        result.replaceChildren(h("div", { class: "box", style: "margin:0" },
          h("div", { class: "dim small" }, "Answered in " + r.seconds.toFixed(1) + " s"),
          h("div", { class: "inc-title", style: "margin-top:4px" }, a.title), h("p", { class: "inc-text" }, a.summary),
          h("p", { class: "inc-text" }, h("b", {}, "Likely cause: "), a.likely_cause),
          h("p", { class: "inc-text" }, h("b", {}, "Suggested: "), a.action, " — ", a.action_reason),
          h("div", { class: "dim small", style: "margin-top:6px" }, a.action === "command:migrate" ? "✔ It picked the right fix (apply migrations)." : "The right fix here is applying migrations; a larger model may do better.")));
      } catch (e) { result.replaceChildren(h("div", { class: "note-box err" }, e.message)); }
      testBtn.disabled = false;
    }, "", "play");
    f.url.onchange = loadModels;
    c.className = "";
    c.replaceChildren(flow.el,
      h("div", { class: "note-box" }, "The monitor reads every target's output as it happens. Rules and learned log patterns pick out failures, crashes, health drops, bursts of errors and new kinds of error, in microseconds per line. Only those moments go to the AI model, which explains them and suggests one fix from a fixed list (restart, pull and redeploy, roll back, a quick command). It never acts by itself: you apply a fix from the Incidents page. The model runs in Ollama on this machine, so your logs stay here."),
      h("div", { class: "box" },
        h("label", { class: "rowf", style: "gap:8px" }, f.enabled, h("b", {}, "Watch logs and open incidents")),
        h("div", { class: "hint", style: "margin:2px 0 14px 24px" }, "Works without AI too: incidents then carry the rule's own description."),
        h("label", { class: "rowf", style: "gap:8px" }, f.ai, h("b", {}, "Explain incidents with a local AI model (Ollama)")),
        h("div", { class: "grid g2", style: "gap:10px;margin-top:10px" }, field("Ollama address", f.url, "Ollama on this machine, or on a trusted computer on your network."), h("div", {}, field("Model", f.model), modelNote)),
        h("label", { class: "rowf", style: "gap:8px;margin-top:6px" }, f.logs, "Include the last log lines in alert emails"),
        h("div", { class: "rowf", style: "margin-top:14px" },
          btn("Save", () => act(() => api("PUT", "/api/settings/monitor", { enabled: f.enabled.checked, ai_enabled: f.ai.checked, ollama_url: f.url.value.trim(), model: f.model.value, email_logs: f.logs.checked }), "Monitor settings saved"), "hl"),
          testBtn, h("a", { class: "btn", href: "#/incidents" }, icon("bell"), "Incidents")),
        result));
    loadModels();
  }, (e) => c.replaceChildren(h("div", { class: "note-box err" }, e.message)));
  return { el: c, cleanup: flow.cleanup };
};
