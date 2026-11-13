//! Alerts by email. Anything worth waking someone for goes through
//! `notify`: incidents from the monitor (`monitor.rs`) and the test button.
//!
//! Throttling keeps a bad night from becoming a flood: the same alert key
//! (target + kind) is sent at most once per `REPEAT_AFTER`, and at most
//! `MAX_PER_HOUR` emails go out in total; suppressed alerts are counted and
//! mentioned in the next email.

use crate::mail::{self, Smtp};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SETTINGS_KEY: &str = "alerts";
const PASSWORD_AAD: &str = "smtp_password";
const REPEAT_AFTER: Duration = Duration::from_secs(30 * 60);
const MAX_PER_HOUR: usize = 12;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, PartialOrd, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    #[default]
    Warning,
    Critical,
}

impl Severity {
    pub fn word(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// Stored alert settings. The SMTP password is kept sealed (`secrets.rs`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub smtp: Smtp,
    /// Lowest severity that is emailed.
    #[serde(default)]
    pub min_severity: Severity,
}

pub async fn load(state: &AppState) -> Settings {
    let mut s: Settings = state
        .db
        .get_setting(SETTINGS_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    s.smtp.password = crate::secrets::open(&s.smtp.password, PASSWORD_AAD).unwrap_or_default();
    s
}

/// Save `new`. An empty password keeps the stored one.
pub async fn save(state: &AppState, mut new: Settings) -> Result<(), String> {
    new.smtp.to.retain(|t| !t.trim().is_empty());
    if new.enabled {
        mail::check(&new.smtp)?;
    }
    if new.smtp.password.is_empty() {
        new.smtp.password = load(state).await.smtp.password;
    }
    new.smtp.password = crate::secrets::seal(&new.smtp.password, PASSWORD_AAD);
    let json = serde_json::to_string(&new).map_err(|e| e.to_string())?;
    state.db.set_setting(SETTINGS_KEY, Some(json)).await
}

/// The settings for the dashboard: everything but the password.
pub fn public_view(s: &Settings) -> serde_json::Value {
    let mut smtp = serde_json::to_value(&s.smtp).unwrap_or_default();
    smtp["password"] = json!("");
    json!({
        "enabled": s.enabled,
        "smtp": smtp,
        "password_set": !s.smtp.password.is_empty(),
        "min_severity": s.min_severity,
    })
}

pub struct Alert {
    pub severity: Severity,
    /// Throttling key, e.g. `"12:crash"`.
    pub key: String,
    pub subject: String,
    pub body: String,
    /// The incident's trace, which records what happened to this email.
    pub trace: Option<String>,
}

#[derive(Default)]
pub struct Throttle {
    last_by_key: HashMap<String, Instant>,
    sent: Vec<Instant>,
    suppressed: usize,
}

impl Throttle {
    /// May an alert with `key` go out at `now`? Records it if so.
    fn admit(&mut self, key: &str, now: Instant) -> Result<usize, ()> {
        self.sent.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
        let repeat = self.last_by_key.get(key).is_some_and(|t| now.duration_since(*t) < REPEAT_AFTER);
        if repeat || self.sent.len() >= MAX_PER_HOUR {
            self.suppressed += 1;
            return Err(());
        }
        self.last_by_key.insert(key.to_string(), now);
        self.sent.push(now);
        Ok(std::mem::take(&mut self.suppressed))
    }
}

static THROTTLE: Mutex<Option<Throttle>> = Mutex::new(None);

/// Email `alert` if alerts are on, it is severe enough and not throttled.
/// Runs in the background; the result goes to Activity.
pub fn notify(state: &Arc<AppState>, alert: Alert) {
    let state = state.clone();
    tokio::spawn(async move {
        let settings = load(&state).await;
        let note = |text: String| {
            let (state, trace) = (state.clone(), alert.trace.clone());
            async move {
                if let Some(t) = trace {
                    let _ = state.db.trace_note(t, "email", text).await;
                }
            }
        };
        if !settings.enabled {
            note("not sent: email alerts are off".into()).await;
            return;
        }
        if alert.severity < settings.min_severity {
            note(format!("not sent: below the {} threshold", settings.min_severity.word())).await;
            return;
        }
        let earlier_suppressed = {
            let mut guard = THROTTLE.lock().unwrap_or_else(|p| p.into_inner());
            guard.get_or_insert_with(Throttle::default).admit(&alert.key, Instant::now()).ok()
        };
        let Some(earlier_suppressed) = earlier_suppressed else {
            note("not sent: the same problem was emailed less than 30 minutes ago, or the hourly limit was reached".into()).await;
            return;
        };
        let mut body = alert.body.clone();
        if earlier_suppressed > 0 {
            body.push_str(&format!(
                "\n\n({earlier_suppressed} similar alert(s) were not emailed to avoid flooding your inbox; \
                 see Activity and Incidents in PortalControl.)"
            ));
        }
        let subject = format!("[PortalControl] {}", alert.subject);
        let result = mail::send(&settings.smtp, &subject, &body).await;
        let to = settings.smtp.to.join(", ");
        match &result {
            Ok(()) => {
                state.activity("info", "alert", None, format!("Alert emailed: {}", alert.subject));
                note(format!("sent to {to} at {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))).await;
                crate::monitor::with_stats(|s| s.emails += 1);
            }
            Err(e) => {
                state.activity("warn", "alert", None, format!("Could not email an alert: {e}"));
                note(format!("failed: {e}")).await;
            }
        }
        state.emit(json!({"type": "monitor", "stage": "email", "ok": result.is_ok(), "trace": alert.trace}));
    });
}

/// Send a test email with the given settings (not throttled).
pub async fn send_test(settings: &Settings, host: &str) -> Result<(), String> {
    let body = format!(
        "This is a test from PortalControl on {host}.\n\nAlerts are working: you will get an email like this \
         when something needs attention (severity {} and above).",
        settings.min_severity.word()
    );
    mail::send(&settings.smtp, "[PortalControl] Test alert", &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_limits_repeats_and_volume() {
        let mut t = Throttle::default();
        let now = Instant::now();
        assert_eq!(t.admit("1:crash", now), Ok(0));
        assert!(
            t.admit("1:crash", now + Duration::from_secs(60)).is_err(),
            "same problem: not again so soon"
        );
        assert_eq!(
            t.admit("2:crash", now),
            Ok(1),
            "a different target goes out and reports the suppressed one"
        );
        assert!(
            t.admit("1:crash", now + REPEAT_AFTER + Duration::from_secs(1)).is_ok(),
            "after the quiet period"
        );
        for i in 0..20 {
            let _ = t.admit(&format!("k{i}"), now + Duration::from_secs(120));
        }
        assert!(t.admit("another", now + Duration::from_secs(130)).is_err(), "hourly cap");
    }

    #[test]
    fn severity_order() {
        assert!(Severity::Critical > Severity::Warning && Severity::Warning > Severity::Info);
    }
}
