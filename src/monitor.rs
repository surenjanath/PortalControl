//! The log monitor: watches every target's output and events in real time,
//! opens an *incident* when something looks wrong, has a local AI model
//! (Ollama) explain it and pick a fix, and emails an alert.
//!
//! It is a two-stage pipeline, so the model never reads the raw stream:
//!
//! 1. **Rules and learned patterns** (this file, microseconds per line):
//!    - a run failed, a service crashed or keeps crashing, a health check
//!      went DOWN, a deploy or schedule failed (from the activity events);
//!    - an *error burst*: many error lines within a minute, well above that
//!      target's own learned baseline;
//!    - a *new kind of error*: every line is reduced to a template (numbers,
//!      ids and hex become placeholders) and the templates a target normally
//!      prints are learned; after a learning period, an error line with a
//!      template never seen before is flagged.
//! 2. **The model**, only for flagged moments: given the target, what
//!    triggered, the recent log lines and a fixed list of actions it may
//!    suggest, it answers in a JSON schema: severity, title, summary, likely
//!    cause, one suggested action (or none). Without a model, or when it is
//!    unreachable, the incident is still recorded and alerted with the rule's
//!    own description.
//!
//! The model suggests; it never acts. A suggested action is a button on the
//! Incidents page that a person clicks (`api/incidents.rs`).
//!
//! It listens to the same event stream as the dashboards, so the supervisor's
//! own code is not involved, and it ignores its own `ai` and `alert` events.

use crate::alerts::{self, Severity};
use crate::db::Incident;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

pub const SETTINGS_KEY: &str = "monitor";

/// Running counters since the manager started, for the live view
/// (`GET /api/monitor/stats`) and its pipeline diagram.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    /// Output lines read.
    pub lines: u64,
    /// Lines that looked like errors (went past the first filter).
    pub error_lines: u64,
    /// Rule hits that opened an incident.
    pub flagged: u64,
    pub ai_calls: u64,
    pub ai_failed: u64,
    pub ai_ms_total: u64,
    pub last_ai_ms: Option<u64>,
    pub emails: u64,
    pub last_trace: Option<String>,
    /// Requests read out of the output, and rule hits of the attack
    /// detection (`threat.rs`).
    pub requests: u64,
    pub threats: u64,
    /// Per target: (lines read, error lines, patterns learned, still learning).
    pub targets: HashMap<i64, (u64, u64, usize, bool)>,
}

pub static STATS: std::sync::Mutex<Option<Stats>> = std::sync::Mutex::new(None);

pub fn with_stats(f: impl FnOnce(&mut Stats)) {
    let mut guard = STATS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(Stats::default));
}

pub fn stats() -> Stats {
    STATS.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or_default()
}

/// A new trace id: `TR-20261113-230910-7f3a` (local time, then random).
pub fn new_trace_id() -> String {
    format!("TR-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), crate::auth::random_hex(2))
}

/// Tell live views about a step of the pipeline.
fn stage(state: &AppState, stage: &str, extra: Value) {
    let mut ev = json!({"type": "monitor", "stage": stage});
    if let (Some(o), Some(e)) = (ev.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    state.emit(ev);
}

/// Lines kept per target for context.
const CONTEXT_LINES: usize = 400;
/// Lines given to the model, and put in an email.
const MODEL_LINES: usize = 80;
const EMAIL_LINES: usize = 15;
/// Learned templates per target (the oldest learning stops past this).
const MAX_TEMPLATES: usize = 5000;
/// A new error pattern counts only after this much observation.
const LEARN_FOR: Duration = Duration::from_secs(10 * 60);
const LEARN_LINES: u64 = 100;
/// Error lines within `BURST_WINDOW` that make a burst (and at least
/// `BURST_FACTOR` times the learned per-minute baseline).
const BURST_MIN: usize = 10;
const BURST_WINDOW: Duration = Duration::from_secs(60);
const BURST_FACTOR: f64 = 4.0;
/// The same trigger on the same target opens at most one incident per this.
const COOLDOWN: Duration = Duration::from_secs(15 * 60);
const MODEL_TIMEOUT: Duration = Duration::from_secs(180);
/// For a suspected attack: less waiting, fewer raw lines (the evidence
/// carries the facts), more of the evidence in the email.
const SECURITY_MODEL_TIMEOUT: Duration = Duration::from_secs(75);
const SECURITY_RAW_LINES: usize = 30;
const EMAIL_EVIDENCE_LINES: usize = 45;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Rules and incidents. On by default: cheap, local, no network.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Ask the model to analyse incidents.
    #[serde(default)]
    pub ai_enabled: bool,
    #[serde(default = "default_url")]
    pub ollama_url: String,
    #[serde(default)]
    pub model: String,
    /// Put the last log lines in alert emails.
    #[serde(default = "yes")]
    pub email_logs: bool,
}

fn yes() -> bool {
    true
}
fn default_url() -> String {
    crate::ollama::DEFAULT_URL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            ai_enabled: false,
            ollama_url: default_url(),
            model: String::new(),
            email_logs: true,
        }
    }
}

pub async fn load(state: &AppState) -> Settings {
    state
        .db
        .get_setting(SETTINGS_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

pub async fn save(state: &AppState, s: &Settings) -> Result<(), String> {
    if s.ai_enabled && s.model.trim().is_empty() {
        return Err("choose a model".into());
    }
    if s.ai_enabled {
        crate::ollama::models(&s.ollama_url).await?;
    }
    state.db.set_setting(SETTINGS_KEY, Some(serde_json::to_string(s).map_err(|e| e.to_string())?)).await
}

// ------------------------------------------------------------- stage 1 --

/// Reduce a log line to its shape: ids, hex and numbers become placeholders,
/// so `GET /orders/1234 took 35ms` and `GET /orders/99 took 4ms` match.
pub fn template(line: &str) -> String {
    let mut out = String::with_capacity(line.len().min(300));
    for (i, token) in line.chars().take(300).collect::<String>().split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let digits = token.chars().filter(char::is_ascii_digit).count();
        let alnum = token.chars().filter(char::is_ascii_alphanumeric).count();
        if digits > 0 && alnum >= 6 && token.chars().all(|c| c.is_ascii_hexdigit() || "-_.:".contains(c)) {
            out.push_str("<id>");
            continue;
        }
        let mut last_digit = false;
        for c in token.chars() {
            if c.is_ascii_digit() {
                if !last_digit {
                    out.push('#');
                }
                last_digit = true;
            } else {
                out.push(c);
                last_digit = false;
            }
        }
    }
    out
}

/// Does this line report a problem?
pub fn is_error_line(line: &str) -> bool {
    let l = line.to_lowercase();
    const WORDS: [&str; 16] = [
        "error",
        "exception",
        "traceback",
        "fatal",
        "panic",
        "critical",
        "segmentation fault",
        "out of memory",
        "unhandled",
        "refused",
        "timed out",
        "timeout",
        "denied",
        "failed",
        "failure",
        "killed",
    ];
    if WORDS.iter().any(|w| l.contains(w)) {
        // "0 errors", "error=0", "errors: 0" are good news.
        let benign = ["0 error", "error=0", "errors: 0", "errors=0", "0 failed", "failed=0", "no errors"];
        return !benign.iter().any(|b| l.contains(b));
    }
    // An HTTP 5xx in an access log: `" 500 ` or `HTTP/1.1" 502`.
    l.split('"').skip(1).any(|part| {
        let code = part.split_whitespace().next().unwrap_or("");
        code.len() == 3 && code.starts_with('5') && code.chars().all(|c| c.is_ascii_digit())
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerKind {
    Failure,
    CrashLoop,
    HealthDown,
    DeployFailed,
    ErrorBurst,
    NewError,
    /// From the attack detection (`threat.rs`).
    Attack,
    BruteForce,
    Flood,
}

impl TriggerKind {
    pub fn id(&self) -> &'static str {
        match self {
            TriggerKind::Failure => "failure",
            TriggerKind::CrashLoop => "crash_loop",
            TriggerKind::HealthDown => "health_down",
            TriggerKind::DeployFailed => "deploy_failed",
            TriggerKind::ErrorBurst => "error_burst",
            TriggerKind::NewError => "new_error",
            TriggerKind::Attack => "attack",
            TriggerKind::BruteForce => "brute_force",
            TriggerKind::Flood => "flood",
        }
    }
    /// Someone doing this on purpose, as opposed to something breaking.
    pub fn is_security(&self) -> bool {
        matches!(self, TriggerKind::Attack | TriggerKind::BruteForce | TriggerKind::Flood)
    }
    /// The severity when no model gives one.
    fn default_severity(&self) -> Severity {
        match self {
            TriggerKind::CrashLoop | TriggerKind::HealthDown | TriggerKind::Flood => Severity::Critical,
            _ => Severity::Warning,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub target_id: i64,
    pub kind: TriggerKind,
    /// What the rule saw, in words.
    pub what: String,
    /// Attack detection only: the request history, summarised for the
    /// investigation.
    pub evidence: String,
    /// The rule's own severity, when it knows better than the kind's default.
    pub severity: Option<Severity>,
}

impl Trigger {
    pub fn new(target_id: i64, kind: TriggerKind, what: String) -> Self {
        Self { target_id, kind, what, evidence: String::new(), severity: None }
    }

    fn from_alarm(target_id: i64, alarm: crate::threat::Alarm) -> Self {
        let kind = match alarm.kind {
            crate::threat::Kind::Attack => TriggerKind::Attack,
            crate::threat::Kind::BruteForce => TriggerKind::BruteForce,
            crate::threat::Kind::Flood => TriggerKind::Flood,
        };
        Self {
            target_id,
            kind,
            what: alarm.what,
            evidence: alarm.evidence,
            severity: Some(if alarm.critical { Severity::Critical } else { Severity::Warning }),
        }
    }
}

/// What the monitor has learned about one target's output.
#[derive(Default)]
pub struct Watch {
    lines: VecDeque<String>,
    templates: HashMap<String, u32>,
    started: Option<Instant>,
    seen_lines: u64,
    seen_errors: u64,
    errors: VecDeque<Instant>,
    /// Error lines per minute, smoothed.
    baseline: f64,
    minute_start: Option<Instant>,
    minute_errors: usize,
    last: HashMap<&'static str, Instant>,
}

impl Watch {
    /// (lines read, error lines, patterns learned, still learning) for the live view.
    pub fn summary(&self, now: Instant) -> (u64, u64, usize, bool) {
        let learned = self.started.is_some_and(|s| now.duration_since(s) >= LEARN_FOR)
            && self.seen_lines >= LEARN_LINES;
        (self.seen_lines, self.seen_errors, self.templates.len(), !learned)
    }

    pub fn recent(&self, n: usize) -> Vec<String> {
        self.lines.iter().rev().take(n).rev().cloned().collect()
    }

    fn cooled(&mut self, kind: &TriggerKind, now: Instant) -> bool {
        let ok = self.last.get(kind.id()).is_none_or(|t| now.duration_since(*t) >= COOLDOWN);
        if ok {
            self.last.insert(kind.id(), now);
        }
        ok
    }

    /// Feed one line; returns a trigger when it, with what came before, is worth an incident.
    pub fn line(&mut self, target_id: i64, line: &str, now: Instant) -> Option<Trigger> {
        let started = *self.started.get_or_insert(now);
        self.seen_lines += 1;
        self.lines.push_back(line.chars().take(1000).collect());
        while self.lines.len() > CONTEXT_LINES {
            self.lines.pop_front();
        }
        // Per-minute baseline of error lines (smoothed; quiet minutes count as zero).
        let minute_start = *self.minute_start.get_or_insert(now);
        if now.duration_since(minute_start) >= Duration::from_secs(60) {
            let minutes = (now.duration_since(minute_start).as_secs() / 60).min(60);
            self.baseline = 0.85 * self.baseline + 0.15 * self.minute_errors as f64;
            for _ in 1..minutes {
                self.baseline *= 0.85;
            }
            self.minute_start = Some(now);
            self.minute_errors = 0;
        }

        let shape = template(line);
        let known = self.templates.contains_key(&shape);
        if self.templates.len() < MAX_TEMPLATES || known {
            *self.templates.entry(shape).or_insert(0) += 1;
        }
        if !is_error_line(line) {
            return None;
        }
        self.seen_errors += 1;
        self.minute_errors += 1;
        self.errors.push_back(now);
        while self.errors.front().is_some_and(|t| now.duration_since(*t) > BURST_WINDOW) {
            self.errors.pop_front();
        }

        let learned = now.duration_since(started) >= LEARN_FOR && self.seen_lines >= LEARN_LINES;
        let burst = self.errors.len() >= BURST_MIN
            && self.errors.len() as f64 >= BURST_FACTOR * self.baseline.max(1.0);
        if burst && self.cooled(&TriggerKind::ErrorBurst, now) {
            return Some(Trigger::new(
                target_id,
                TriggerKind::ErrorBurst,
                format!(
                    "{} error lines in the last minute (usually about {:.1} per minute)",
                    self.errors.len(),
                    self.baseline
                ),
            ));
        }
        if learned && !known && self.cooled(&TriggerKind::NewError, now) {
            return Some(Trigger::new(
                target_id,
                TriggerKind::NewError,
                format!("a kind of error this process has not printed before: {}", line.trim()),
            ));
        }
        None
    }
}

/// Map an activity event to a trigger, if it is one.
pub fn from_activity(level: &str, kind: &str, target_id: Option<i64>, message: &str) -> Option<Trigger> {
    let target_id = target_id?;
    if level != "error" {
        return None;
    }
    let k = match kind {
        // A quick command someone just ran by hand is not an incident.
        "run" if message.starts_with("Command") => return None,
        "run" => TriggerKind::Failure,
        "restart" if message.contains("suspended") => TriggerKind::CrashLoop,
        "restart" => TriggerKind::Failure,
        "health" => TriggerKind::HealthDown,
        "deploy" | "schedule" => TriggerKind::DeployFailed,
        _ => return None,
    };
    Some(Trigger::new(target_id, k, message.to_string()))
}

// ------------------------------------------------------------- stage 2 --

/// An action the model may suggest, as offered to it.
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

/// The actions that make sense for this target right now.
pub async fn choices(state: &Arc<AppState>, t: &crate::db::Target) -> Vec<Choice> {
    let mut out = Vec::new();
    if t.run_mode == "SERVICE" {
        out.push(Choice { id: "restart".into(), label: format!("Restart {}", t.name) });
    } else {
        out.push(Choice { id: "restart".into(), label: format!("Run {} again", t.name) });
    }
    let git = state.git.lock().ok().and_then(|m| m.get(&t.id).cloned());
    if let Some(g) = git.as_ref().filter(|g| g.is_repo) {
        if g.behind > 0 {
            out.push(Choice {
                id: "deploy".into(),
                label: format!("Pull {} new commit(s) and redeploy", g.behind),
            });
        }
        // The last commit that deployed successfully, if it is not the current one.
        if let Ok(runs) = state.db.list_runs(t.id, 30).await {
            let good = runs.iter().find(|r| {
                r.status == "SUCCESS"
                    && r.commit_sha.is_some()
                    && r.commit_sha.as_deref() != g.head.as_deref()
            });
            if let Some(sha) = good.and_then(|r| r.commit_sha.clone()) {
                let msg = good.and_then(|r| r.commit_message.clone()).unwrap_or_default();
                out.push(Choice {
                    id: format!("rollback:{sha}"),
                    label: format!(
                        "Roll back to {} ({})",
                        &sha[..sha.len().min(8)],
                        msg.lines().next().unwrap_or("")
                    ),
                });
            }
        }
    }
    for (id, label) in crate::api::command_choices(t) {
        out.push(Choice { id: format!("command:{id}"), label: format!("Run \"{label}\"") });
    }
    out
}

pub const SYSTEM_PROMPT: &str = "You are the incident analyst inside PortalControl, a tool that runs and supervises \
software projects (web servers, workers, scheduled jobs) on one machine. You are given one incident: the process, \
what the monitoring rules saw, its recent log lines, and the list of actions an operator can take. \
Explain what is going on for a busy developer. Be specific and brief; quote the exact error. \
Use only facts from the logs; when they do not show the cause, say so. \
For `action`, choose the single most useful id from the offered actions, or \"none\" when no offered action \
would help (for example when the code must be changed). Never invent other actions. Rules of thumb: \
a missing database table or column means migrations have not been applied (apply migrations if offered); \
a missing module or package means requirements are not installed (install them if offered); \
new commits on origin that fix the problem call for pull and redeploy; a failure that started with the latest \
deployment calls for rolling back; restarting only helps transient failures (a crash, a hang, a lost connection) \
and does not fix code, configuration or schema problems. \
Severity: critical = the service is down or keeps failing; warning = degraded or failing in part; \
info = noise or already recovered.";

/// The model's brief when the rules suspect an attack instead of a fault.
pub const SECURITY_PROMPT: &str = "You are the security analyst inside PortalControl, a tool that runs and supervises \
web applications (mostly Django) on one machine. Its attack detection flagged the requests one application is \
receiving. You are given the rule that fired and the evidence: counts of recent requests, who sent them, what was \
asked for, what the application answered, everything the main sender did, and the latest raw log lines. \
Investigate it like an incident responder and report for a busy administrator. Work only from the evidence; \
when it names the sender this incident is about, report on that sender only: what other senders did in the same minutes is separate activity and must not be attributed to this one or merged into this incident. A `user=` on a row is the account that was signed in for that request, not who the sender claims to be. \
The raw log lines at the end contain every sender; use them only to check a detail. Django's message \
\"You may need to add ... to ALLOWED_HOSTS\" is its standard hint: for a forged Host header that name must \
NOT be added. \
When the evidence does not show something, say so. Decide and state: (1) what kind of activity this is (for example SQL \
injection attempts, path traversal, a vulnerability scanner, password guessing, a flood meant to overload the \
application) or that it is harmless (a monitoring tool, a misbehaving script or page of their own, a search engine); \
(2) who is doing it: the address, and whether it is on the local network (10.x, 172.16-31.x, 192.168.x, 100.64-127.x) \
or outside, and the signed-in user if there is one; (3) whether anything succeeded. Read the answers: 404, 403 and \
400 mean the application refused; a 500 means the request broke something and may have found a weakness; a 2xx \
answer to an injection or to a secret file such as /.env means it may have worked; a sign-in accepted (302) after \
many failures means a password was probably guessed. Put the kind of activity and the sender in `title`, what \
happened and whether it succeeded in `summary`, and what they were after and how you can tell in `likely_cause`. \
In `action_reason` say what the administrator should do now: for example block the address at the firewall or \
router, change the password of the account and end its sessions, remove or protect the exposed file, fix the \
code that the request broke, or nothing. For `action` choose an offered id only when it really helps, such as a \
restart to recover an application that a flood has made unresponsive; restarting does not stop an attacker, so \
usually answer \"none\". Severity: critical = something succeeded, the application is being overloaded, or an \
automated attack is in progress; warning = attempts that the application refused; info = harmless.";

pub fn security_prompt(
    t: &crate::db::Target,
    trigger: &Trigger,
    extra: &str,
    lines: &[String],
    actions: &[Choice],
) -> String {
    let offered: Vec<String> = actions.iter().map(|a| format!("- {}: {}", a.id, a.label)).collect();
    format!(
        "Application: {name} in project {project}\nFolder: {dir}\n{extra}\n\
         Attack detection rule ({kind}): {what}\n\nEvidence:\n{evidence}\n\n\
         Offered actions:\n{offered}\n- none: no action\n\nLatest raw log lines (oldest first):\n{log}",
        name = t.name,
        project = t.project,
        dir = t.working_directory,
        kind = trigger.kind.id(),
        what = trigger.what,
        evidence = trigger.evidence,
        offered = offered.join("\n"),
        log = if lines.is_empty() { "(none)".to_string() } else { lines.join("\n") },
    )
}

pub fn answer_schema(actions: &[Choice]) -> Value {
    let mut ids: Vec<String> = actions.iter().map(|a| a.id.clone()).collect();
    ids.push("none".into());
    // The order matters (serde_json keeps it): the model writes its analysis
    // first and picks the action last, with the analysis in front of it.
    json!({
        "type": "object",
        "properties": {
            "severity": {"type": "string", "enum": ["info", "warning", "critical"]},
            "title": {"type": "string", "description": "at most 80 characters"},
            "summary": {"type": "string", "description": "what is happening, 1-3 sentences"},
            "likely_cause": {"type": "string", "description": "the most likely cause, or that the logs do not show it"},
            "key_error": {"type": "string", "description": "the one log line that shows the problem, copied exactly, or empty"},
            "steps": {
                "type": "array",
                "items": {"type": "string"},
                "description": "1 to 5 concrete steps for the operator, in order",
            },
            "confidence": {"type": "string", "enum": ["low", "medium", "high"]},
            "action": {"type": "string", "enum": ids},
            "action_reason": {"type": "string", "description": "why this action, or what to change by hand"},
        },
        "required": [
            "severity", "title", "summary", "likely_cause", "key_error", "steps", "confidence", "action",
            "action_reason"
        ],
    })
}

pub fn prompt(
    t: &crate::db::Target,
    trigger: &Trigger,
    extra: &str,
    lines: &[String],
    actions: &[Choice],
) -> String {
    let offered: Vec<String> = actions.iter().map(|a| format!("- {}: {}", a.id, a.label)).collect();
    format!(
        "Process: {name} ({mode}) in project {project}\nFolder: {dir}\nScript: {script}\n{extra}\n\
         Monitoring rule triggered ({kind}): {what}\n\nOffered actions:\n{offered}\n- none: no action\n\n\
         Recent log lines (oldest first):\n{log}",
        name = t.name,
        mode = if t.run_mode == "SERVICE" { "long-running service" } else { "task that runs to completion" },
        project = t.project,
        dir = t.working_directory,
        script = t.script_name,
        kind = trigger.kind.id(),
        what = trigger.what,
        offered = offered.join("\n"),
        log = if lines.is_empty() { "(no output captured)".to_string() } else { lines.join("\n") },
    )
}

fn clean(text: &str, max: usize) -> String {
    let t: String = text.trim().chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect();
    t
}

/// Turn the model's answer into incident fields, keeping only offered actions.
pub fn apply_answer(inc: &mut Incident, answer: &Value, actions: &[Choice]) {
    let s = |k: &str| answer.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    if let sev @ ("info" | "warning" | "critical") = s("severity").as_str() {
        inc.severity = sev.to_string();
    }
    if !s("title").trim().is_empty() {
        inc.title = clean(&s("title"), 120);
    }
    inc.summary = clean(&s("summary"), 1500);
    inc.cause = clean(&s("likely_cause"), 1500);
    let action = s("action");
    match actions.iter().find(|a| a.id == action) {
        Some(a) => {
            inc.action = a.id.clone();
            inc.action_label = a.label.clone();
        }
        None => {
            inc.action = "none".into();
            inc.action_label.clear();
        }
    }
    inc.action_reason = clean(&s("action_reason"), 800);
    // The extra fields of 0.14.0 go into the incident's details.
    let steps: Vec<String> = answer
        .get("steps")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|t| clean(t, 300))
                .filter(|t| !t.is_empty())
                .take(6)
                .collect()
        })
        .unwrap_or_default();
    let confidence = s("confidence");
    if !inc.details.is_object() {
        inc.details = json!({});
    }
    inc.details["ai"] = json!({
        "key_error": clean(&s("key_error"), 400),
        "steps": steps,
        "confidence": matches!(confidence.as_str(), "low" | "medium" | "high").then_some(confidence),
    });
}

/// The log lines to show the model: whichever source has the most of the
/// recent output. The monitor's own window can be short (just after a
/// restart); a running process's live buffer has its output so far; a
/// finished run's stored log has everything it printed.
async fn context_lines(state: &Arc<AppState>, target_id: i64, watched: Vec<String>) -> Vec<String> {
    if watched.len() >= 20 {
        return watched;
    }
    let tail_of = |text: &str| -> Vec<String> {
        let lines: Vec<String> =
            text.lines().filter(|l| !l.starts_with("[portalcontrol]")).map(str::to_string).collect();
        lines[lines.len().saturating_sub(MODEL_LINES)..].to_vec()
    };
    let live = {
        let running = state.running.lock().await;
        running.get(&target_id).and_then(|h| h.tail.lock().ok().map(|t| t.joined()))
    };
    let stored = match state.db.list_runs(target_id, 1).await.ok().and_then(|r| r.first().map(|r| r.id)) {
        Some(run) => state.db.get_run(run).await.ok().flatten().and_then(|r| r.captured_logs),
        None => None,
    };
    [Some(watched), live.map(|t| tail_of(&t)), stored.map(|t| tail_of(&t))]
        .into_iter()
        .flatten()
        .max_by_key(Vec::len)
        .unwrap_or_default()
}

async fn handle(
    state: Arc<AppState>,
    trigger: Trigger,
    watched: Vec<String>,
    trace_id: String,
    run_id: Option<i64>,
) {
    let Ok(Some(t)) = state.db.get_target(trigger.target_id).await else { return };
    // An event without output (a crash, a health change): use the latest run.
    let run_id = match run_id {
        Some(r) => Some(r),
        None => state.db.list_runs(t.id, 1).await.ok().and_then(|r| r.first().map(|r| r.id)),
    };
    let mut trace = crate::db::Trace {
        trace_id: trace_id.clone(),
        target_id: Some(t.id),
        run_id,
        trigger: trigger.kind.id().into(),
        what: trigger.what.clone(),
        status: "rules".into(),
        ..Default::default()
    };
    let settings = load(&state).await;
    let security = trigger.kind.is_security();
    let lines = context_lines(&state, t.id, watched).await;
    let health = state.health.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let running = state.running.lock().await.contains_key(&t.id);
    let extra = format!(
        "Now: {}; health check: {}",
        if running { "running" } else { "not running" },
        health
            .as_ref()
            .map(|h| format!("{} {}", h.status, h.detail.clone().unwrap_or_default()))
            .unwrap_or_else(|| "none".into())
    );

    let mut inc = Incident {
        target_id: Some(t.id),
        trigger: trigger.kind.id().into(),
        severity: trigger.severity.unwrap_or_else(|| trigger.kind.default_severity()).word().into(),
        title: match trigger.kind {
            TriggerKind::Attack => format!("Possible attack on {}", t.name),
            TriggerKind::BruteForce => format!("Password guessing on {}", t.name),
            TriggerKind::Flood => format!("Flood of requests on {}", t.name),
            TriggerKind::Failure => format!("{} failed", t.name),
            TriggerKind::CrashLoop => format!("{} keeps crashing", t.name),
            TriggerKind::HealthDown => format!("{} is not answering its health check", t.name),
            TriggerKind::DeployFailed => format!("Deploying {} failed", t.name),
            TriggerKind::ErrorBurst => format!("Burst of errors in {}", t.name),
            TriggerKind::NewError => format!("New kind of error in {}", t.name),
        },
        summary: trigger.what.clone(),
        // An attack is shown by its evidence, not by the last output lines.
        excerpt: if security { trigger.evidence.clone() } else { lines.join("\n") },
        action: "none".into(),
        trace_id: trace_id.clone(),
        run_id,
        ..Default::default()
    };

    let actions = choices(&state, &t).await;

    // Everything known about this moment, kept with the incident (0.14.0):
    // what ran, the health and Git state, the key error, how often it
    // happened before, and the rules' own diagnosis.
    let diag = crate::diagnose::diagnose(trigger.kind.id(), &lines, &actions);
    let run = match run_id {
        Some(r) => state.db.get_run(r).await.ok().flatten(),
        None => None,
    };
    let git = state.git.lock().ok().and_then(|m| m.get(&t.id).cloned());
    let now = crate::util::now_secs();
    let since = |secs: u64| crate::util::fmt_utc(now.saturating_sub(secs));
    let kind_id = trigger.kind.id().to_string();
    let (same_24h, previous_at) =
        state.db.similar_incidents(t.id, kind_id.clone(), since(86_400)).await.unwrap_or((0, None));
    let (same_7d, _) =
        state.db.similar_incidents(t.id, kind_id, since(7 * 86_400)).await.unwrap_or((0, None));
    let key_error = if security { None } else { crate::diagnose::key_error(&lines) };
    let first_error: Option<String> = if security {
        None
    } else {
        lines.iter().find(|l| is_error_line(l)).map(|l| l.trim().chars().take(400).collect())
    };
    inc.details = json!({
        "context": {
            "project": t.project,
            "mode": t.run_mode,
            "running": running,
            "health_url": t.health_url,
            "health": health.as_ref().map(|h| json!({
                "status": h.status, "latency_ms": h.latency_ms, "http_status": h.http_status,
                "detail": h.detail, "checked_at": h.checked_at,
            })),
            "git": git.as_ref().filter(|g| g.is_repo).map(|g| json!({
                "branch": g.checked_out_branch, "head": g.head, "head_summary": g.head_summary,
                "behind": g.behind, "ahead": g.ahead, "dirty_files": g.dirty_files,
            })),
        },
        "run": run.as_ref().map(|r| json!({
            "id": r.id, "triggered_by": r.triggered_by, "status": r.status, "exit_code": r.exit_code,
            "duration_ms": r.duration_ms, "executed_at": r.executed_at, "commit_sha": r.commit_sha,
            "commit_message": r.commit_message, "label": r.label,
        })),
        "signals": {
            "rule": trigger.what,
            "lines": lines.len(),
            "error_lines": if security { 0 } else { lines.iter().filter(|l| is_error_line(l)).count() },
            "key_error": key_error,
            "first_error": first_error,
        },
        "history": {"same_24h": same_24h, "same_7d": same_7d, "previous_at": previous_at},
        "diagnosis": diag.clone(),
        "actions_offered": actions.clone(),
        "source": "rules",
    });
    // A recognised error is a useful hint for a small model.
    let extra = match &diag.matched {
        Some(line) => format!(
            "{extra}\nThe monitoring rules recognise this as \"{}\" from the line: {line}",
            diag.title
        ),
        None => extra,
    };

    if settings.ai_enabled && !settings.model.is_empty() {
        let (system, user) = if security {
            let raw = &lines[lines.len().saturating_sub(SECURITY_RAW_LINES)..];
            (SECURITY_PROMPT, security_prompt(&t, &trigger, &extra, raw, &actions))
        } else {
            let recent = &lines[lines.len().saturating_sub(MODEL_LINES)..];
            (SYSTEM_PROMPT, prompt(&t, &trigger, &extra, recent, &actions))
        };
        trace.model = settings.model.clone();
        trace.prompt = format!("[system]\n{system}\n\n[user]\n{user}");
        stage(&state, "ai_start", json!({"target_id": t.id, "trace": trace_id, "model": settings.model}));
        let answer = crate::ollama::chat_json(
            &settings.ollama_url,
            &settings.model,
            system,
            &user,
            &answer_schema(&actions),
            // An attack is reported fast: the alert does not wait long for the model.
            if security { SECURITY_MODEL_TIMEOUT } else { MODEL_TIMEOUT },
        )
        .await;
        match answer {
            Ok((answer, usage)) => {
                apply_answer(&mut inc, &answer, &actions);
                inc.analysed_by = format!("{} ({:.1} s)", settings.model, usage.took.as_secs_f64());
                let ms = usage.took.as_millis() as u64;
                trace.status = "ok".into();
                trace.answer = usage.raw;
                trace.duration_ms = Some(ms as i64);
                trace.prompt_tokens = usage.prompt_tokens;
                trace.answer_tokens = usage.answer_tokens;
                with_stats(|s| {
                    s.ai_calls += 1;
                    s.ai_ms_total += ms;
                    s.last_ai_ms = Some(ms);
                });
                stage(
                    &state,
                    "ai_done",
                    json!({"target_id": t.id, "trace": trace_id, "ok": true, "ms": ms, "action": inc.action}),
                );
            }
            Err(e) => {
                inc.note = format!("AI analysis unavailable: {e}");
                trace.status = "failed".into();
                trace.error = e.clone();
                with_stats(|s| {
                    s.ai_calls += 1;
                    s.ai_failed += 1;
                });
                stage(
                    &state,
                    "ai_done",
                    json!({"target_id": t.id, "trace": trace_id, "ok": false, "error": e}),
                );
                state.activity("warn", "ai", Some(t.id), format!("{trace_id}: AI analysis failed: {e}"));
            }
        }
    }

    // Without an answer from the model, the rules' diagnosis explains it.
    let answered = trace.status == "ok";
    inc.details["source"] = json!(if answered {
        "ai"
    } else if settings.ai_enabled && !settings.model.is_empty() {
        "rules_ai_failed"
    } else {
        "rules"
    });
    if !answered {
        inc.cause = diag.cause.clone();
        inc.action_reason = diag.steps.join(" ");
        if diag.matched.is_some() {
            inc.title = format!("{} \u{2014} {}", inc.title, diag.title.to_lowercase());
        }
        if let Some(a) = diag.action.as_ref().and_then(|id| actions.iter().find(|c| &c.id == id)) {
            inc.action = a.id.clone();
            inc.action_label = a.label.clone();
        }
    }

    let id = match state.db.insert_incident(inc.clone()).await {
        Ok(id) => id,
        Err(e) => {
            crate::util::log(&format!("could not store an incident: {e}"));
            return;
        }
    };
    let level = match inc.severity.as_str() {
        "critical" => "error",
        "warning" => "warn",
        _ => "info",
    };
    trace.incident_id = Some(id);
    if let Err(e) = state.db.insert_trace(trace).await {
        crate::util::log(&format!("could not store trace {trace_id}: {e}"));
    }
    with_stats(|s| s.last_trace = Some(trace_id.clone()));
    state.activity(level, "ai", Some(t.id), format!("Incident #{id} ({trace_id}): {}", inc.title));
    state.emit(
        json!({"type": "incident", "id": id, "target_id": t.id, "severity": inc.severity, "trace": trace_id}),
    );

    let severity = match inc.severity.as_str() {
        "critical" => Severity::Critical,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    };
    alerts::notify(
        &state,
        alerts::Alert {
            severity,
            key: format!("{}:{}", t.id, trigger.kind.id()),
            trace: Some(trace_id.clone()),
            subject: format!("{} \u{2014} {}", severity.word().to_uppercase(), inc.title),
            body: email_body(
                &state,
                id,
                &t,
                &trigger,
                &inc,
                &evidence_or(&trigger, &lines),
                settings.email_logs,
            ),
        },
    );
}

/// The lines an email carries: the evidence of an attack, else the output.
fn evidence_or(trigger: &Trigger, lines: &[String]) -> Vec<String> {
    if trigger.kind.is_security() {
        trigger.evidence.lines().map(str::to_string).collect()
    } else {
        lines.to_vec()
    }
}

fn email_body(
    state: &AppState,
    id: i64,
    t: &crate::db::Target,
    trigger: &Trigger,
    inc: &Incident,
    lines: &[String],
    with_logs: bool,
) -> String {
    let mut b = format!("{}\n{} (project {})\n\n", inc.title, t.name, t.project);
    if !inc.summary.is_empty() {
        b.push_str(&format!("What happened: {}\n\n", inc.summary));
    }
    if !inc.cause.is_empty() {
        b.push_str(&format!("Likely cause: {}\n\n", inc.cause));
    }
    let key = inc.details["ai"]["key_error"]
        .as_str()
        .filter(|k| !k.is_empty())
        .or_else(|| inc.details["signals"]["key_error"].as_str());
    if let Some(k) = key {
        b.push_str(&format!("Key error: {k}\n\n"));
    }
    let steps: Vec<&str> = inc.details["ai"]["steps"]
        .as_array()
        .filter(|a| !a.is_empty())
        .or_else(|| inc.details["diagnosis"]["steps"].as_array())
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !steps.is_empty() {
        b.push_str("Steps:\n");
        for (n, s) in steps.iter().enumerate() {
            b.push_str(&format!("  {}. {s}\n", n + 1));
        }
        b.push('\n');
    }
    if inc.action != "none" {
        b.push_str(&format!(
            "Suggested fix: {}. {}\nNothing has been changed: apply it from the Incidents page.\n\n",
            inc.action_label, inc.action_reason
        ));
    } else if !inc.action_reason.is_empty() {
        b.push_str(&format!("What to do: {}\n\n", inc.action_reason));
    }
    b.push_str(&format!("Detected by: {} \u{2014} {}\n", trigger.kind.id(), trigger.what));
    b.push_str(&format!(
        "Analysed by: {}\n",
        if inc.analysed_by.is_empty() {
            "monitoring rules only (no AI model set up)".to_string()
        } else {
            format!("{} on this machine", inc.analysed_by)
        }
    ));
    b.push_str(&format!("Incident #{id}: http://{}/#/incidents\n", state.bind));
    b.push_str(&format!(
        "Trace: {} (log of run #{})\n",
        inc.trace_id,
        inc.run_id.map(|r| r.to_string()).unwrap_or_else(|| "-".into())
    ));
    if with_logs && !lines.is_empty() {
        let security = trigger.kind.is_security();
        b.push_str(if security { "\nEvidence:\n" } else { "\nLast log lines:\n" });
        // The evidence starts with its summary, so it is cut at the end.
        let shown = if security {
            &lines[..lines.len().min(EMAIL_EVIDENCE_LINES)]
        } else {
            &lines[lines.len().saturating_sub(EMAIL_LINES)..]
        };
        for l in shown {
            b.push_str("  ");
            b.push_str(l);
            b.push('\n');
        }
    }
    b
}

/// Background worker: read the event stream, run stage 1, hand triggers to stage 2.
pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut rx = state.events.subscribe();
        let mut watches: HashMap<i64, Watch> = HashMap::new();
        let mut trackers: HashMap<i64, crate::threat::Tracker> = HashMap::new();
        let mut last_run: HashMap<i64, i64> = HashMap::new();
        let mut enabled = load(&state).await.enabled;
        let mut checked = Instant::now();
        loop {
            let raw = match rx.recv().await {
                Ok(m) => m,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            // Pick up a change in Settings within half a minute.
            if checked.elapsed() > Duration::from_secs(30) {
                enabled = load(&state).await.enabled;
                checked = Instant::now();
            }
            if !enabled {
                continue;
            }
            let Ok(m) = serde_json::from_str::<Value>(&raw) else { continue };
            let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let target = m.get("target_id").and_then(Value::as_i64);
            let mut triggers: Vec<Trigger> = Vec::new();
            let trigger = match s("type").as_str() {
                "log" => {
                    let (Some(id), line) = (target, s("line")) else { continue };
                    // PortalControl's own notes in the log are not the program's output.
                    if line.starts_with("[portalcontrol]") || line.starts_with("[scan]") {
                        continue;
                    }
                    if let Some(run) = m.get("run_id").and_then(Value::as_i64).filter(|r| *r > 0) {
                        last_run.insert(id, run);
                    }
                    let now = Instant::now();
                    // Attack detection: the request in the line, or a
                    // security message of the application itself.
                    let tracker = trackers.entry(id).or_default();
                    let request = crate::threat::parse_request(&line);
                    let is_request = request.is_some();
                    let alarm = match request {
                        Some(r) => {
                            let clock = chrono::Local::now().format("%H:%M:%S").to_string();
                            tracker.request(r, now, &clock)
                        }
                        None => tracker.signal(&line, now),
                    };
                    if is_request || alarm.is_some() {
                        with_stats(|s| {
                            s.requests += u64::from(is_request);
                            s.threats += u64::from(alarm.is_some());
                        });
                    }
                    if let Some(alarm) = alarm {
                        // Said at once; the investigation follows as an incident.
                        state.activity(
                            if alarm.critical { "error" } else { "warn" },
                            "security",
                            Some(id),
                            format!("Attack detection: {}", alarm.what),
                        );
                        triggers.push(Trigger::from_alarm(id, alarm));
                    }
                    // The request log of the middleware is data for the attack
                    // detection, not program output to learn error patterns from.
                    if line.contains(crate::threat::MARK) {
                        with_stats(|s| s.lines += 1);
                        continue_with(&state, &mut triggers, &watches, &last_run);
                        continue;
                    }
                    let w = watches.entry(id).or_default();
                    let hit = w.line(id, &line, now);
                    let summary = w.summary(now);
                    let error = is_error_line(&line);
                    with_stats(|s| {
                        s.lines += 1;
                        if error {
                            s.error_lines += 1;
                        }
                        s.targets.insert(id, summary);
                    });
                    hit
                }
                "activity" => {
                    let kind = s("kind");
                    if kind == "ai" || kind == "alert" {
                        continue;
                    }
                    from_activity(&s("level"), &kind, target, &s("message"))
                        .filter(|t| watches.entry(t.target_id).or_default().cooled(&t.kind, Instant::now()))
                }
                "targets_changed" => {
                    // Forget deleted targets.
                    if let Ok(list) = state.db.list_targets().await {
                        watches.retain(|id, _| list.iter().any(|t| t.id == *id));
                        trackers.retain(|id, _| list.iter().any(|t| t.id == *id));
                        with_stats(|s| s.targets.retain(|id, _| list.iter().any(|t| t.id == *id)));
                    }
                    None
                }
                _ => None,
            };
            triggers.extend(trigger);
            continue_with(&state, &mut triggers, &watches, &last_run);
        }
    });
}

/// Hand what the rules flagged to stage 2, each as its own task.
fn continue_with(
    state: &Arc<AppState>,
    triggers: &mut Vec<Trigger>,
    watches: &HashMap<i64, Watch>,
    last_run: &HashMap<i64, i64>,
) {
    for trigger in triggers.drain(..) {
        let recent = watches.get(&trigger.target_id).map(|w| w.recent(MODEL_LINES)).unwrap_or_default();
        let trace_id = new_trace_id();
        with_stats(|s| s.flagged += 1);
        stage(
            state,
            "flagged",
            json!({"target_id": trigger.target_id, "trigger": trigger.kind.id(), "what": trigger.what, "trace": trace_id}),
        );
        let run = last_run.get(&trigger.target_id).copied();
        tokio::spawn(handle(state.clone(), trigger, recent, trace_id, run));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_ids_are_unique_and_readable() {
        let a = new_trace_id();
        let b = new_trace_id();
        assert!(a.starts_with("TR-") && a.len() == "TR-20261113-230910-7f3a".len(), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn templates_ignore_numbers_and_ids() {
        assert_eq!(template("GET /orders/1234 took 35ms"), template("GET /orders/9 took 4ms"));
        assert_eq!(template("commit 1d46d7e5a1 deployed"), "commit <id> deployed");
        assert_ne!(template("connection refused"), template("disk full"));
    }

    #[test]
    fn error_lines() {
        assert!(is_error_line("Traceback (most recent call last):"));
        assert!(is_error_line("psycopg2.OperationalError: connection refused"));
        assert!(is_error_line(r#"127.0.0.1 - "GET /api HTTP/1.1" 502 0"#));
        assert!(!is_error_line(r#"127.0.0.1 - "GET /api HTTP/1.1" 200 512"#));
        assert!(!is_error_line("12 passed, 0 failed"));
        assert!(!is_error_line("Applying shop.0003... OK"));
    }

    #[test]
    fn bursts_need_volume_and_a_jump_over_the_baseline() {
        let mut w = Watch::default();
        let t0 = Instant::now();
        for i in 0..9 {
            assert!(w.line(1, "ERROR db timeout", t0 + Duration::from_secs(i)).is_none());
        }
        let t = w.line(1, "ERROR db timeout", t0 + Duration::from_secs(10)).expect("burst");
        assert_eq!(t.kind, TriggerKind::ErrorBurst);
        assert!(w.line(1, "ERROR db timeout", t0 + Duration::from_secs(11)).is_none(), "cooldown");
    }

    #[test]
    fn a_new_kind_of_error_after_learning() {
        let mut w = Watch::default();
        let t0 = Instant::now();
        // Learn: normal traffic and one familiar warning, for 11 minutes.
        for i in 0..200u64 {
            let at = t0 + Duration::from_secs(i * 4);
            w.line(1, &format!("GET /item/{i} 200 in {}ms", i % 50), at);
            if i % 40 == 0 {
                w.line(1, "WARNING cache timeout, retrying", at);
            }
        }
        let later = t0 + Duration::from_secs(900);
        assert!(
            w.line(1, "WARNING cache timeout, retrying", later).is_none(),
            "a known error shape is not new"
        );
        let t = w.line(1, "KeyError: 'customer_id'", later).expect("new error");
        assert_eq!(t.kind, TriggerKind::NewError);
        // Not before learning:
        let mut fresh = Watch::default();
        assert!(fresh.line(2, "KeyError: 'x'", t0).is_none());
    }

    #[test]
    fn activity_events_map_to_triggers() {
        assert_eq!(
            from_activity("error", "run", Some(1), "Deployment #4 failed").unwrap().kind,
            TriggerKind::Failure
        );
        assert!(from_activity("error", "run", Some(1), "Command \"migrate\" #5 failed").is_none());
        assert_eq!(
            from_activity(
                "error",
                "restart",
                Some(1),
                "Service crashed 5 times; automatic restart suspended"
            )
            .unwrap()
            .kind,
            TriggerKind::CrashLoop
        );
        assert_eq!(
            from_activity("error", "health", Some(1), "Health changed").unwrap().kind,
            TriggerKind::HealthDown
        );
        assert!(from_activity("ok", "run", Some(1), "finished").is_none());
        assert!(from_activity("error", "run", None, "x").is_none());
    }

    #[test]
    fn the_model_can_only_pick_offered_actions() {
        let actions = vec![Choice { id: "restart".into(), label: "Restart web".into() }];
        let mut inc = Incident { severity: "warning".into(), action: "none".into(), ..Default::default() };
        apply_answer(
            &mut inc,
            &json!({"severity": "critical", "title": "DB down", "summary": "s", "likely_cause": "c",
                    "action": "rm -rf /", "action_reason": "r"}),
            &actions,
        );
        assert_eq!(inc.action, "none", "an action that was not offered is dropped");
        assert_eq!(inc.severity, "critical");
        apply_answer(&mut inc, &json!({"severity": "bogus", "action": "restart"}), &actions);
        assert_eq!((inc.action.as_str(), inc.severity.as_str()), ("restart", "critical"));
        let schema = answer_schema(&actions);
        assert_eq!(schema["properties"]["action"]["enum"], json!(["restart", "none"]));
        let order: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(
            order,
            [
                "severity",
                "title",
                "summary",
                "likely_cause",
                "key_error",
                "steps",
                "confidence",
                "action",
                "action_reason"
            ],
            "analysis before action"
        );
        apply_answer(
            &mut inc,
            &json!({"steps": ["Apply migrations", " ", "Restart"], "confidence": "sure", "key_error": "E"}),
            &actions,
        );
        assert_eq!(inc.details["ai"]["steps"], json!(["Apply migrations", "Restart"]));
        assert_eq!(inc.details["ai"]["confidence"], Value::Null, "only low, medium or high");
    }
}
