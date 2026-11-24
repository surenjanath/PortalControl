//! Security posture (added in 0.14.0): what can be improved, worked out
//! from PortalControl's own settings and from each target's scans,
//! configuration, repository and attack incidents. It is shown on the
//! Security page and makes up the security assessment report.
//!
//! Everything here is a pure function of the facts passed in, so it can be
//! tested without a running manager. Environment values are read to check
//! them (for example that DEBUG is off); they are never put in a finding.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    High,
    Medium,
    Low,
}

impl Level {
    pub fn word(self) -> &'static str {
        match self {
            Level::High => "High",
            Level::Medium => "Medium",
            Level::Low => "Low",
        }
    }
    fn points(self) -> u32 {
        match self {
            Level::High => 15,
            Level::Medium => 6,
            Level::Low => 2,
        }
    }
}

/// One thing to improve.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    /// `PortalControl` or the target's name.
    pub area: String,
    pub target_id: Option<i64>,
    pub title: String,
    /// What was found, without secrets.
    pub detail: String,
    /// What to do about it.
    pub fix: String,
}

/// A check of PortalControl's own configuration, passed or not.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// A package with known vulnerabilities, from pip-audit.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VulnPackage {
    pub name: String,
    pub version: String,
    /// CVE ids where pip-audit knows them, else its own advisory ids.
    pub advisories: Vec<String>,
    /// The lowest version that fixes every advisory, if there is one.
    pub fix: Option<String>,
}

/// What a scan found, reduced for the posture and the report.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ScanFacts {
    pub scanned_at: String,
    pub age_days: i64,
    pub high: i64,
    pub medium: i64,
    pub low: i64,
    pub packages: Vec<VulnPackage>,
    pub bandit_error: bool,
    pub audit_error: bool,
}

/// PortalControl's own settings that matter for security.
#[derive(Debug, Clone, Default)]
pub struct Platform {
    pub password_set: bool,
    /// The dashboard is served on a network address, not only on 127.0.0.1.
    pub network_open: bool,
    pub network_https: bool,
    pub allow_other_networks: bool,
    pub network_expires: bool,
    pub monitor_enabled: bool,
    pub alerts_enabled: bool,
}

/// Facts about one target.
#[derive(Debug, Clone, Default)]
pub struct TargetFacts {
    pub id: i64,
    pub name: String,
    pub service: bool,
    pub scan: Option<ScanFacts>,
    /// Stored variables and those of the project's `.env`, for the checks only.
    pub env: Vec<(String, String)>,
    /// Env files committed to the repository.
    pub tracked_env_files: Vec<String>,
    pub webhook: bool,
    pub webhook_secret: bool,
    pub git_dirty: bool,
    pub attacks_30d: usize,
    pub attacks_open: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Posture {
    pub score: u32,
    pub grade: &'static str,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub findings: Vec<Finding>,
    pub checks: Vec<Check>,
}

/// Scans older than this are reported as stale.
pub const STALE_DAYS: i64 = 30;

/// Reduce a stored scan payload (`security.rs`) to its facts.
pub fn scan_facts(payload: &Value, scanned_at: &str, now: i64) -> ScanFacts {
    let (high, medium, low) = payload.get("bandit").map(crate::security::bandit_counts).unwrap_or((0, 0, 0));
    let deps = payload
        .get("pip_audit")
        .and_then(|p| p.get("dependencies").or(Some(p)))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let packages = deps
        .iter()
        .filter_map(|d| {
            let vulns = d.get("vulns").and_then(Value::as_array).filter(|v| !v.is_empty())?;
            let mut advisories: Vec<String> = vulns
                .iter()
                .map(|v| {
                    let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let cve = v
                        .get("aliases")
                        .and_then(Value::as_array)
                        .and_then(|a| a.iter().filter_map(Value::as_str).find(|x| x.starts_with("CVE-")))
                        .map(str::to_string);
                    if id.starts_with("CVE-") {
                        id
                    } else {
                        cve.unwrap_or(id)
                    }
                })
                .filter(|a| !a.is_empty())
                .collect();
            advisories.sort();
            advisories.dedup();
            // The fix that covers every advisory is the highest of their lowest fixes.
            let fix = vulns
                .iter()
                .filter_map(|v| {
                    v.get("fix_versions").and_then(Value::as_array).and_then(|f| {
                        f.iter()
                            .filter_map(Value::as_str)
                            .min_by(|a, b| version_cmp(a, b))
                            .map(str::to_string)
                    })
                })
                .max_by(|a, b| version_cmp(a, b));
            Some(VulnPackage {
                name: d.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
                version: d.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
                advisories,
                fix,
            })
        })
        .collect();
    let age_days = chrono::NaiveDateTime::parse_from_str(scanned_at.trim(), "%Y-%m-%d %H:%M:%S")
        .map(|t| (now - t.and_utc().timestamp()).max(0) / 86_400)
        .unwrap_or(0);
    ScanFacts {
        scanned_at: scanned_at.to_string(),
        age_days,
        high,
        medium,
        low,
        packages,
        bandit_error: payload.get("bandit_error").is_some(),
        audit_error: payload.get("pip_audit_error").is_some(),
    }
}

/// Compare dotted versions by their numbers (`2.10` after `2.9`).
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

/// Variables of a `.env` text, for the checks: `KEY=value`, `export KEY=value`,
/// quotes removed, comments skipped.
pub fn parse_env_text(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim().trim_matches(['"', '\'']).to_string();
            Some((k.trim().to_string(), v))
        })
        .collect()
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn platform_checks(p: &Platform, out: &mut Vec<Finding>) -> Vec<Check> {
    let mut add = |level, title: &str, detail: &str, fix: &str| {
        out.push(Finding {
            level,
            area: "PortalControl".into(),
            target_id: None,
            title: title.into(),
            detail: detail.into(),
            fix: fix.into(),
        })
    };
    if !p.password_set {
        add(
            Level::Medium,
            "The dashboard has no password",
            "Anyone who can use this computer can open the dashboard, start any script and read the logs. Other computers are refused until a password is set.",
            "Set a password under Settings > Access.",
        );
    }
    if p.network_open && !p.network_https {
        add(
            Level::Medium,
            "The dashboard is served over plain HTTP on the network",
            "The password and the session cookie travel unencrypted between the browser and this computer.",
            "Turn on HTTPS with a certificate under Settings > Network.",
        );
    }
    if p.network_open && p.allow_other_networks {
        add(
            Level::Medium,
            "Computers from other networks may connect",
            "The dashboard answers computers outside this computer's own network.",
            "Allow only the local network under Settings > Network, or use a VPN such as Tailscale.",
        );
    }
    if p.network_open && !p.network_expires {
        add(
            Level::Low,
            "Network access has no end time",
            "The dashboard stays open to the network until it is switched off.",
            "Open it for a limited time when it is needed (Settings > Network).",
        );
    }
    if !p.monitor_enabled {
        add(
            Level::Medium,
            "The log monitor is off",
            "Attacks, crashes and failures are not detected and no incidents are opened.",
            "Switch the monitor on under Settings > AI Monitor.",
        );
    }
    if !p.alerts_enabled {
        add(
            Level::Low,
            "No alert emails",
            "Nobody is told about critical incidents or attacks unless the dashboard is open.",
            "Set up Settings > Email Alerts.",
        );
    }
    let on_network = |yes: &str, no: &str| if p.network_open { yes.to_string() } else { no.to_string() };
    vec![
        Check {
            name: "Dashboard password",
            ok: p.password_set,
            detail: if p.password_set { "set" } else { "not set" }.into(),
        },
        Check {
            name: "Network exposure",
            ok: !p.network_open || !p.allow_other_networks,
            detail: if p.allow_other_networks {
                on_network("open to other networks", "this computer only (127.0.0.1)")
            } else {
                on_network("local network only", "this computer only (127.0.0.1)")
            },
        },
        Check {
            name: "Encryption on the network",
            ok: !p.network_open || p.network_https,
            detail: on_network(if p.network_https { "HTTPS" } else { "plain HTTP" }, "not on the network"),
        },
        Check {
            name: "Time limit on network access",
            ok: !p.network_open || p.network_expires,
            detail: on_network(
                if p.network_expires { "ends automatically" } else { "no end time" },
                "not on the network",
            ),
        },
        Check {
            name: "Stored secrets encrypted",
            ok: true,
            detail: "environment variables and webhook secrets are sealed (ChaCha20-Poly1305)".into(),
        },
        Check {
            name: "Log monitor and attack detection",
            ok: p.monitor_enabled,
            detail: if p.monitor_enabled { "on" } else { "off" }.into(),
        },
        Check {
            name: "Alert emails",
            ok: p.alerts_enabled,
            detail: if p.alerts_enabled { "on" } else { "off" }.into(),
        },
    ]
}

fn target_findings(t: &TargetFacts, out: &mut Vec<Finding>) {
    let mut add = |level, title: String, detail: String, fix: &str| {
        out.push(Finding {
            level,
            area: t.name.clone(),
            target_id: Some(t.id),
            title,
            detail,
            fix: fix.into(),
        })
    };
    match &t.scan {
        None => add(
            Level::Medium,
            "Never scanned".into(),
            "Its code and dependencies have not been checked for known problems.".into(),
            "Run a scan from the Security page.",
        ),
        Some(s) => {
            if s.high > 0 {
                add(
                    Level::High,
                    format!("{} high-severity code issue(s)", s.high),
                    "Static analysis (bandit) found code that is likely exploitable, such as injection or unsafe deserialisation.".into(),
                    "Fix each issue listed under the application in this report, then scan again.",
                );
            }
            if s.medium > 0 {
                add(
                    Level::Medium,
                    format!("{} medium-severity code issue(s)", s.medium),
                    "Static analysis (bandit) found code that may be unsafe.".into(),
                    "Review the issues; fix them, or mark false positives with # nosec.",
                );
            }
            if !s.packages.is_empty() {
                let names: Vec<String> = s
                    .packages
                    .iter()
                    .take(6)
                    .map(|p| match &p.fix {
                        Some(f) => format!("{} {} (fixed in {f})", p.name, p.version),
                        None => format!("{} {} (no fix yet)", p.name, p.version),
                    })
                    .collect();
                let advisories: usize = s.packages.iter().map(|p| p.advisories.len()).sum();
                add(
                    Level::High,
                    format!(
                        "{} package(s) with known vulnerabilities ({advisories} advisories)",
                        s.packages.len()
                    ),
                    names.join(", "),
                    "Upgrade them to the fixed versions in requirements.txt, install, test and redeploy.",
                );
            }
            if s.bandit_error || s.audit_error {
                add(
                    Level::Low,
                    "Part of the scan did not run".into(),
                    if s.bandit_error { "Static analysis failed." } else { "The dependency audit failed." }
                        .into(),
                    "Open the target's Security tab for the error, fix it and scan again.",
                );
            }
            if s.age_days > STALE_DAYS {
                add(
                    Level::Low,
                    format!("Last scan is {} days old", s.age_days),
                    "New advisories are published every day; an old scan misses them.".into(),
                    "Scan again, at least monthly.",
                );
            }
        }
    }
    let get =
        |k: &str| t.env.iter().rev().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    if t.service {
        if let Some(key) =
            ["DEBUG", "DJANGO_DEBUG", "FLASK_DEBUG"].iter().find(|k| get(k).is_some_and(truthy))
        {
            add(
                Level::High,
                "Debug mode is on".into(),
                format!("{key} is switched on for a service. Error pages then show code, settings and data to every visitor."),
                "Set it to False (or remove it) for anything other users can reach.",
            );
        }
    }
    if let Some(key) = ["SECRET_KEY", "DJANGO_SECRET_KEY"].iter().find(|k| {
        get(k).is_some_and(|v| v.starts_with("django-insecure") || (!v.is_empty() && v.chars().count() < 32))
    }) {
        add(
            Level::High,
            "Weak secret key".into(),
            format!("{key} is the development default or shorter than 32 characters; sessions and password-reset links can be forged with it."),
            "Generate a long random key and store it under Environment Variables.",
        );
    }
    if get("ALLOWED_HOSTS")
        .or_else(|| get("DJANGO_ALLOWED_HOSTS"))
        .is_some_and(|v| v.split([',', ' ']).any(|h| h.trim() == "*"))
    {
        add(
            Level::Medium,
            "ALLOWED_HOSTS accepts any host name".into(),
            "With '*' Django answers forged Host headers, which enables cache poisoning and poisoned password-reset links.".into(),
            "List the real host names only.",
        );
    }
    if !t.tracked_env_files.is_empty() {
        add(
            Level::High,
            "Secrets are committed to Git".into(),
            format!(
                "{} {} in the repository, so the secrets in it are in its history and on every clone.",
                t.tracked_env_files.join(", "),
                if t.tracked_env_files.len() == 1 { "is" } else { "are" }
            ),
            "Remove the file from Git (git rm --cached), add it to .gitignore, and change every secret it held.",
        );
    }
    if t.webhook && !t.webhook_secret {
        add(
            Level::Medium,
            "Deploy webhook without a signing secret".into(),
            "Anyone who learns the webhook URL can start a deployment.".into(),
            "Add a signing secret under Configuration > Webhooks and in the Git host's webhook settings.",
        );
    }
    if t.attacks_open > 0 {
        add(
            Level::High,
            format!("{} open security incident(s)", t.attacks_open),
            format!("{} attack(s) detected in the last 30 days.", t.attacks_30d),
            "Review them on the Incidents page: check whether anything succeeded and block the senders.",
        );
    } else if t.attacks_30d > 0 {
        add(
            Level::Low,
            format!("{} attack(s) in the last 30 days, all closed", t.attacks_30d),
            "The application is being probed.".into(),
            "Consider rate limiting and blocking repeat senders at the firewall.",
        );
    }
    if t.git_dirty {
        add(
            Level::Low,
            "Uncommitted changes on the server".into(),
            "The code that runs is not exactly what is in Git, so it cannot be reviewed or restored from there.".into(),
            "Commit the changes, or undo them under Configuration > Git Source.",
        );
    }
}

/// Work out the posture. Findings are sorted most serious first.
pub fn assess(platform: &Platform, targets: &[TargetFacts]) -> Posture {
    let mut findings = Vec::new();
    let checks = platform_checks(platform, &mut findings);
    for t in targets {
        target_findings(t, &mut findings);
    }
    findings.sort_by(|a, b| a.level.cmp(&b.level).then(a.area.cmp(&b.area)));
    let lost: u32 = findings.iter().map(|f| f.level.points()).sum();
    let score = 100u32.saturating_sub(lost);
    let grade = match score {
        90.. => "A",
        80..=89 => "B",
        70..=79 => "C",
        60..=69 => "D",
        _ => "F",
    };
    let count = |l: Level| findings.iter().filter(|f| f.level == l).count();
    Posture {
        score,
        grade,
        high: count(Level::High),
        medium: count(Level::Medium),
        low: count(Level::Low),
        findings,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn safe_platform() -> Platform {
        Platform { password_set: true, monitor_enabled: true, alerts_enabled: true, ..Default::default() }
    }

    #[test]
    fn a_clean_setup_scores_full_marks() {
        let t = TargetFacts {
            id: 1,
            name: "web".into(),
            service: true,
            scan: Some(ScanFacts { scanned_at: "x".into(), ..Default::default() }),
            env: vec![("DEBUG".into(), "False".into())],
            ..Default::default()
        };
        let p = assess(&safe_platform(), &[t]);
        assert_eq!((p.score, p.grade, p.findings.len()), (100, "A", 0));
        assert!(p.checks.iter().all(|c| c.ok));
    }

    #[test]
    fn risky_configuration_is_found_without_showing_secrets() {
        let t = TargetFacts {
            id: 1,
            name: "shop".into(),
            service: true,
            env: vec![
                ("DEBUG".into(), "True".into()),
                ("SECRET_KEY".into(), "django-insecure-hunter2".into()),
                ("ALLOWED_HOSTS".into(), "example.com, *".into()),
            ],
            tracked_env_files: vec![".env".into()],
            webhook: true,
            attacks_30d: 3,
            attacks_open: 1,
            ..Default::default()
        };
        let p = assess(&Platform::default(), &[t]);
        let titles: Vec<&str> = p.findings.iter().map(|f| f.title.as_str()).collect();
        for want in [
            "Debug mode is on",
            "Weak secret key",
            "ALLOWED_HOSTS accepts any host name",
            "Secrets are committed to Git",
            "Deploy webhook without a signing secret",
            "Never scanned",
            "The dashboard has no password",
        ] {
            assert!(titles.contains(&want), "missing {want}: {titles:?}");
        }
        assert!(p.findings.iter().all(|f| !f.detail.contains("hunter2")), "no secret values");
        assert_eq!(p.findings[0].level, Level::High, "most serious first");
        assert_eq!(p.grade, "F");
    }

    #[test]
    fn pip_audit_payloads_give_cve_ids_and_the_fix_that_covers_all() {
        let payload = json!({
            "bandit": {"results": [{"issue_severity": "HIGH"}, {"issue_severity": "LOW"}]},
            "pip_audit": {"dependencies": [
                {"name": "django", "version": "4.2.0", "vulns": [
                    {"id": "PYSEC-2023-100", "aliases": ["GHSA-x", "CVE-2023-31047"], "fix_versions": ["4.2.1", "3.2.19"]},
                    {"id": "GHSA-y", "aliases": [], "fix_versions": ["4.2.10"]}
                ]},
                {"name": "requests", "version": "2.31.0", "vulns": []}
            ]}
        });
        // 2026-11-11 10:00:00 UTC is ten days after the scan.
        let f = scan_facts(&payload, "2026-11-01 10:00:00", 1_794_391_200);
        assert_eq!((f.high, f.low, f.age_days), (1, 1, 10));
        assert_eq!(f.packages.len(), 1);
        assert_eq!(f.packages[0].advisories, ["CVE-2023-31047", "GHSA-y"]);
        assert_eq!(f.packages[0].fix.as_deref(), Some("4.2.10"), "the highest of the lowest fixes");
    }

    #[test]
    fn env_text_is_read_like_dotenv() {
        let env = parse_env_text("# c\nexport DEBUG=\"true\"\nSECRET_KEY='abc'\n\nBROKEN");
        assert_eq!(
            env,
            [("DEBUG".to_string(), "true".to_string()), ("SECRET_KEY".to_string(), "abc".to_string())]
        );
    }
}
