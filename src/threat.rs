//! Attack detection: is someone trying to break into, or knock over, one of
//! the supervised applications?
//!
//! It works on the same output lines as the log monitor (`monitor.rs`), as a
//! first stage that costs microseconds per line:
//!
//! 1. **Read the request out of the line.** Three shapes are understood:
//!    the `PCREQ {json}` line printed by PortalControl's Django middleware
//!    (`integrations/django/portalcontrol_requestlog.py`: address, user,
//!    browser, host, time taken), the common/combined access-log format of
//!    gunicorn, uvicorn and nginx (address and usually the browser), and
//!    Django's own `runserver` line (no address).
//! 2. **Rules** over a short history of those requests, per target:
//!    - attack signatures in the address asked for (SQL injection, path
//!      traversal, script and command injection), and scanner programs;
//!    - probing for secret files and for other software (`/.env`,
//!      `/wp-login.php`), and sweeps that collect many 4xx answers;
//!    - repeated failed sign-ins, and a sign-in that succeeds after them;
//!    - a flood of requests far above the target's usual rate;
//!    - Django's own security messages (forged Host header, failed CSRF
//!      checks, `SuspiciousOperation`).
//! 3. **Evidence for the investigation.** When a rule fires, the history is
//!    summarised: who sent what, how often, what the application answered,
//!    and everything the main source did. That text is what the AI model
//!    reads (`monitor.rs`), and what the incident shows.
//!
//! Nothing here blocks anyone. It reports; a person decides.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Starts a line printed by the Django middleware.
pub const MARK: &str = "PCREQ ";
/// The middleware itself, offered for download from the dashboard.
pub const DJANGO_MIDDLEWARE: &str = include_str!("../integrations/django/portalcontrol_requestlog.py");

/// Requests remembered per target, and for how long.
const KEEP: usize = 3000;
const KEEP_FOR: Duration = Duration::from_secs(30 * 60);
/// Most rules count within this window.
const WINDOW: Duration = Duration::from_secs(5 * 60);
/// Probe requests in the window that make an incident.
const PROBE_MIN: usize = 5;
/// Attack requests in the window that mean an automated scan is running.
const SCAN_MIN: usize = 20;
/// 4xx answers within a minute that mean someone is sweeping for addresses.
const SWEEP_MIN: usize = 40;
const SWEEP_WINDOW: Duration = Duration::from_secs(60);
/// Failed sign-ins in the window.
const LOGIN_FAILS: usize = 8;
/// 401 answers in the window (guessing tokens or passwords of an API).
const UNAUTHORISED_MIN: usize = 20;
/// A flood: this many requests in `FLOOD_WINDOW`, and at least `FLOOD_FACTOR`
/// times what the target usually gets in that time.
const FLOOD_WINDOW: Duration = Duration::from_secs(10);
const FLOOD_MIN: usize = 150;
const FLOOD_FACTOR: f64 = 6.0;
/// The same rule on the same target fires at most once per this.
const COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// One request, as far as the log line tells.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// The computer that sent it, when the log says.
    pub ip: Option<String>,
    pub method: String,
    /// Path with the query string.
    pub path: String,
    pub status: u16,
    pub ms: Option<u64>,
    /// Signed-in user, when the log says.
    pub user: String,
    pub agent: String,
    pub referer: String,
    pub host: String,
}

fn is_loopback(ip: &str) -> bool {
    ip.is_empty() || ip.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())
}

fn parse_marked(json: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let direct = s("ip");
    // Behind a proxy on this computer the real sender is in the header.
    let forwarded = s("xff").split(',').next().unwrap_or("").trim().to_string();
    let ip = if is_loopback(&direct) && !forwarded.is_empty() { forwarded } else { direct };
    Some(Request {
        ip: (!ip.is_empty()).then_some(ip),
        method: s("m"),
        path: s("p"),
        status: v.get("s").and_then(Value::as_u64).filter(|c| (100..600).contains(c))? as u16,
        ms: v.get("ms").and_then(Value::as_u64),
        user: s("u"),
        agent: s("ua"),
        referer: s("ref"),
        host: s("host"),
    })
}

const METHODS: [&str; 9] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE"];

/// `1.2.3.4 - - [date] "GET /x HTTP/1.1" 200 12 "referer" "browser"`, and the
/// shorter lines of uvicorn (`1.2.3.4:5123 - "GET ..." 200 OK`) and Django's
/// runserver (`[date] "GET ..." 200 12`).
fn parse_access(line: &str) -> Option<Request> {
    let parts: Vec<&str> = line.split('"').collect();
    let (at, request) =
        parts.iter().enumerate().skip(1).step_by(2).find(|(_, p)| {
            METHODS.iter().any(|m| p.strip_prefix(m).is_some_and(|rest| rest.starts_with(' ')))
        })?;
    let (method, rest) = request.split_once(' ')?;
    let path = rest.rsplit_once(" HTTP/").map(|(p, _)| p).unwrap_or(rest).trim();
    let status: u16 = parts.get(at + 1)?.split_whitespace().next()?.parse().ok()?;
    // A request names a path, a full address (through a proxy) or `*`.
    if !(100..600).contains(&status) || !(path.starts_with('/') || path.starts_with("http") || path == "*") {
        return None;
    }
    let ip = parts[..at].iter().flat_map(|p| p.split_whitespace()).find_map(|token| {
        let token = token.trim_end_matches(':');
        let host = match token.rsplit_once(':') {
            // `1.2.3.4:5123`; an IPv6 address has more than one colon.
            Some((host, port))
                if host.matches(':').count() == 0 && port.chars().all(|c| c.is_ascii_digit()) =>
            {
                host
            }
            _ => token,
        };
        host.parse::<std::net::IpAddr>().ok().map(|_| host.to_string())
    });
    let quoted = |i: usize| parts.get(i).filter(|s| **s != "-").map(|s| s.to_string()).unwrap_or_default();
    Some(Request {
        ip,
        method: method.to_string(),
        path: path.to_string(),
        status,
        referer: quoted(at + 2),
        agent: quoted(at + 4),
        ..Default::default()
    })
}

/// The request a log line reports, if it reports one.
pub fn parse_request(line: &str) -> Option<Request> {
    match line.find(MARK) {
        Some(at) => parse_marked(&line[at + MARK.len()..]),
        None => parse_access(line),
    }
}

/// Undo `%xx` and `+`, and lower the case, so that `%2E%2E%2F` reads `../`.
fn decode(path: &str) -> String {
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        match b[i] {
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) * 16 + hex(b[i + 2]).unwrap_or(0));
                i += 3;
                continue;
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_lowercase()
}

/// What a rule recognised in one request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub label: &'static str,
    /// Clear on its own: one such request is worth reporting.
    pub strong: bool,
}

const SIGNATURES: &[(&str, &[&str])] = &[
    (
        "SQL injection",
        &[
            "union select",
            "union all select",
            "' or '1'='1",
            "' or 1=1",
            " or 1=1--",
            "information_schema",
            "pg_sleep(",
            "sleep(",
            "benchmark(",
            "waitfor delay",
            "xp_cmdshell",
            "load_file(",
            "into outfile",
            "';--",
        ],
    ),
    ("path traversal", &["../", "..\\", "/etc/passwd", "/etc/shadow", "win.ini", "boot.ini", "/proc/self/"]),
    ("script injection (XSS)", &["<script", "javascript:", "onerror=", "onload=", "<svg", "<iframe"]),
    (
        "command injection",
        &[";cat ", "|cat ", ";wget ", "|wget ", ";curl ", "|curl ", "$(", "/bin/sh", "/bin/bash", "cmd.exe"],
    ),
    (
        "remote code execution probe",
        &["${jndi:", "{{7*7}}", "${7*7}", "php://input", "php://filter", "data://text"],
    ),
];

const SECRET_FILES: &[&str] = &[
    "/.env",
    "/.git/",
    "/.aws/",
    "/.ssh/",
    "/.svn/",
    "/.htpasswd",
    "/.ds_store",
    "id_rsa",
    "backup.sql",
    "dump.sql",
    "db.sqlite3",
    "/config.json",
    "/credentials",
];

const OTHER_SOFTWARE: &[&str] = &[
    "/wp-login.php",
    "/wp-admin",
    "/wp-content",
    "/wp-includes",
    "/xmlrpc.php",
    "/phpmyadmin",
    "/pma/",
    "/adminer",
    "/cgi-bin/",
    "/vendor/phpunit",
    "/actuator",
    "/solr/",
    "/owa/",
    "/autodiscover",
    "/server-status",
    "/manager/html",
    "/boaform",
    "/hnap1",
    "/telescope",
    "/_profiler",
    "/shell.php",
    "/eval-stdin.php",
];

const SCANNERS: &[&str] = &[
    "sqlmap",
    "nikto",
    "nmap",
    "masscan",
    "zgrab",
    "nuclei",
    "wpscan",
    "dirbuster",
    "gobuster",
    "feroxbuster",
    "ffuf",
    "wfuzz",
    "hydra",
    "acunetix",
    "nessus",
    "openvas",
    "whatweb",
    "metasploit",
];

/// Does this request look like part of an attack?
pub fn classify(req: &Request) -> Option<Hit> {
    let path = decode(&req.path);
    for (label, needles) in SIGNATURES {
        if needles.iter().any(|n| path.contains(n)) {
            return Some(Hit { label, strong: true });
        }
    }
    let agent = req.agent.to_lowercase();
    if SCANNERS.iter().any(|s| agent.contains(s)) {
        return Some(Hit { label: "scanner program", strong: true });
    }
    if SECRET_FILES.iter().any(|n| path.contains(n)) {
        return Some(Hit { label: "probe for secret files", strong: false });
    }
    if OTHER_SOFTWARE.iter().any(|n| path.contains(n)) {
        return Some(Hit { label: "probe for other software", strong: false });
    }
    // A page type this application does not have.
    let file = path.split(['?', '#']).next().unwrap_or("");
    if req.status == 404 && [".php", ".asp", ".aspx", ".jsp", ".cgi"].iter().any(|e| file.ends_with(e)) {
        return Some(Hit { label: "probe for other software", strong: false });
    }
    None
}

fn is_login(req: &Request) -> bool {
    let path = req.path.split('?').next().unwrap_or("").to_lowercase();
    ["login", "signin", "sign-in", "sign_in", "/auth", "token", "password", "/session"]
        .iter()
        .any(|w| path.contains(w))
}

/// A sign-in form that was sent and not accepted. Django answers a wrong
/// password with the form again (200) and a right one with a redirect.
fn failed_login(req: &Request) -> bool {
    req.method == "POST" && is_login(req) && matches!(req.status, 200 | 400 | 401 | 403 | 429)
}

fn accepted_login(req: &Request) -> bool {
    req.method == "POST" && is_login(req) && matches!(req.status, 301..=303)
}

/// The kind of incident an alarm becomes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Attack,
    BruteForce,
    Flood,
}

/// A rule fired.
#[derive(Debug, Clone)]
pub struct Alarm {
    pub kind: Kind,
    /// Serious enough to wake someone before any analysis.
    pub critical: bool,
    /// What the rule saw, in words.
    pub what: String,
    /// The summary of the request history for the investigation.
    pub evidence: String,
}

struct Seen {
    at: Instant,
    /// Wall-clock time of arrival, for people.
    clock: String,
    req: Request,
    hit: Option<Hit>,
}

/// The request history of one target and the state of its rules.
#[derive(Default)]
pub struct Tracker {
    seen: VecDeque<Seen>,
    /// Security messages the application printed itself: (when, key, line).
    signals: VecDeque<(Instant, &'static str, String)>,
    bucket_start: Option<Instant>,
    bucket: usize,
    /// Requests per `FLOOD_WINDOW`, smoothed.
    baseline: f64,
    last: HashMap<&'static str, Instant>,
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect::<String>() + "\u{2026}"
    }
}

/// The most frequent values, as `a ×12, b ×3`.
fn top(counts: HashMap<String, usize>, n: usize) -> String {
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list.iter().take(n).map(|(k, c)| format!("{k} \u{00d7}{c}")).collect::<Vec<_>>().join(", ")
}

impl Tracker {
    fn cooled(&mut self, key: &'static str, now: Instant) -> bool {
        let ok = self.last.get(key).is_none_or(|t| now.duration_since(*t) >= COOLDOWN);
        if ok {
            self.last.insert(key, now);
        }
        ok
    }

    /// Count the request towards the usual rate. Buckets that are themselves
    /// a flood are not learned, or an attack would become "usual".
    fn tick(&mut self, now: Instant) {
        let start = *self.bucket_start.get_or_insert(now);
        let elapsed = now.duration_since(start);
        if elapsed >= FLOOD_WINDOW {
            if self.bucket < FLOOD_MIN {
                self.baseline = 0.9 * self.baseline + 0.1 * self.bucket as f64;
            }
            let quiet = (elapsed.as_secs() / FLOOD_WINDOW.as_secs()).min(360);
            for _ in 1..quiet {
                self.baseline *= 0.9;
            }
            self.bucket_start = Some(now);
            self.bucket = 0;
        }
        self.bucket += 1;
    }

    fn within(&self, now: Instant, window: Duration) -> impl Iterator<Item = &Seen> {
        self.seen.iter().rev().take_while(move |s| now.duration_since(s.at) <= window)
    }

    fn alarm(&self, kind: Kind, critical: bool, what: String, source: Option<&str>, now: Instant) -> Alarm {
        Alarm { kind, critical, what, evidence: self.evidence(source, kind == Kind::Attack, now) }
    }

    /// Feed one request; `clock` is the time to show for it.
    pub fn request(&mut self, req: Request, now: Instant, clock: &str) -> Option<Alarm> {
        let hit = classify(&req);
        self.tick(now);
        self.seen.push_back(Seen { at: now, clock: clock.to_string(), req: req.clone(), hit });
        while self.seen.len() > KEEP || self.seen.front().is_some_and(|s| now.duration_since(s.at) > KEEP_FOR)
        {
            self.seen.pop_front();
        }

        // Rules count per sender when the log names one, else over everything.
        let source = req.ip.clone();
        let from = source.as_deref().map(|ip| format!(" from {ip}")).unwrap_or_default();
        let same = |s: &Seen| source.is_none() || s.req.ip == source;

        let burst = self.within(now, FLOOD_WINDOW).count();
        if burst >= FLOOD_MIN
            && burst as f64 >= FLOOD_FACTOR * self.baseline.max(1.0)
            && self.cooled("flood", now)
        {
            let mut by_ip: HashMap<String, usize> = HashMap::new();
            for s in self.within(now, FLOOD_WINDOW) {
                *by_ip.entry(s.req.ip.clone().unwrap_or_else(|| "unknown sender".into())).or_insert(0) += 1;
            }
            let main = by_ip.iter().max_by_key(|(_, c)| **c).map(|(ip, c)| (ip.clone(), *c));
            let share = main
                .as_ref()
                .filter(|(ip, _)| ip != "unknown sender")
                .map(|(ip, c)| format!("; {}% of them from {ip}", c * 100 / burst))
                .unwrap_or_default();
            let focus = main.map(|(ip, _)| ip).filter(|ip| ip != "unknown sender");
            return Some(self.alarm(
                Kind::Flood,
                true,
                format!(
                    "{burst} requests in {} seconds (usually about {:.0}){share}",
                    FLOOD_WINDOW.as_secs(),
                    self.baseline
                ),
                focus.as_deref(),
                now,
            ));
        }

        if accepted_login(&req) || failed_login(&req) {
            let fails = self.within(now, WINDOW).filter(|s| same(s) && failed_login(&s.req)).count();
            if accepted_login(&req) && fails >= LOGIN_FAILS && self.cooled("brute_success", now) {
                return Some(self.alarm(
                    Kind::BruteForce,
                    true,
                    format!(
                        "a sign-in at {} was accepted after {fails} failed attempts{from} in the last {} minutes",
                        short(&req.path, 80),
                        WINDOW.as_secs() / 60
                    ),
                    source.as_deref(),
                    now,
                ));
            }
            if fails >= LOGIN_FAILS && self.cooled("brute_force", now) {
                return Some(self.alarm(
                    Kind::BruteForce,
                    false,
                    format!(
                        "{fails} failed sign-ins at {}{from} in the last {} minutes",
                        short(&req.path, 80),
                        WINDOW.as_secs() / 60
                    ),
                    source.as_deref(),
                    now,
                ));
            }
        }
        if req.status == 401 {
            let refused = self.within(now, WINDOW).filter(|s| same(s) && s.req.status == 401).count();
            if refused >= UNAUTHORISED_MIN && self.cooled("brute_force", now) {
                return Some(self.alarm(
                    Kind::BruteForce,
                    false,
                    format!(
                        "{refused} requests{from} were refused as not signed in (401) in the last {} minutes",
                        WINDOW.as_secs() / 60
                    ),
                    source.as_deref(),
                    now,
                ));
            }
        }

        if let Some(hit) = hit {
            let hits = self.within(now, WINDOW).filter(|s| same(s) && s.hit.is_some()).count();
            if hits >= SCAN_MIN && self.cooled("attack_scan", now) {
                return Some(self.alarm(
                    Kind::Attack,
                    true,
                    format!(
                        "{hits} attack requests{from} in the last {} minutes: an automated scan is running",
                        WINDOW.as_secs() / 60
                    ),
                    source.as_deref(),
                    now,
                ));
            }
            if (hit.strong || hits >= PROBE_MIN) && self.cooled("attack", now) {
                let what = if hit.strong {
                    format!(
                        "{}{from}: {} {} was answered with {}",
                        hit.label,
                        req.method,
                        short(&req.path, 160),
                        req.status
                    )
                } else {
                    format!(
                        "{hits} probing requests{from} in the last {} minutes, the latest: {} {}",
                        WINDOW.as_secs() / 60,
                        req.method,
                        short(&req.path, 120)
                    )
                };
                return Some(self.alarm(Kind::Attack, false, what, source.as_deref(), now));
            }
        }

        if matches!(req.status, 400 | 401 | 403 | 404 | 405) {
            let refused = self
                .within(now, SWEEP_WINDOW)
                .filter(|s| same(s) && matches!(s.req.status, 400 | 401 | 403 | 404 | 405))
                .count();
            if refused >= SWEEP_MIN && self.cooled("sweep", now) {
                return Some(self.alarm(
                    Kind::Attack,
                    false,
                    format!(
                        "{refused} requests{from} were answered with 4xx within a minute: someone is trying addresses"
                    ),
                    source.as_deref(),
                    now,
                ));
            }
        }
        None
    }

    /// Feed a line that is not a request: Django reports some attacks itself.
    pub fn signal(&mut self, line: &str, now: Instant) -> Option<Alarm> {
        let (key, label, needed): (&'static str, &str, usize) = if line.contains("Invalid HTTP_HOST header") {
            ("host", "a forged Host header", 3)
        } else if line.contains("Forbidden (CSRF") {
            ("csrf", "a failed CSRF check", 10)
        } else if [
            "SuspiciousOperation",
            "SuspiciousFileOperation",
            "SuspiciousMultipartForm",
            "DisallowedRedirect",
            "RequestDataTooBig",
            "TooManyFieldsSent",
            "TooManyFilesSent",
        ]
        .iter()
        .any(|w| line.contains(w))
        {
            ("suspicious", "a suspicious request", 1)
        } else {
            return None;
        };
        self.signals.push_back((now, key, short(line.trim(), 300)));
        while self.signals.len() > 200
            || self.signals.front().is_some_and(|s| now.duration_since(s.0) > KEEP_FOR)
        {
            self.signals.pop_front();
        }
        let count = self.signals.iter().filter(|s| s.1 == key && now.duration_since(s.0) <= WINDOW).count();
        if count >= needed && self.cooled(key, now) {
            let what = format!(
                "the application reported {label} {count} time(s) in the last {} minutes: {}",
                WINDOW.as_secs() / 60,
                short(line.trim(), 200)
            );
            return Some(Alarm {
                kind: Kind::Attack,
                critical: false,
                what,
                evidence: self.evidence(None, false, now),
            });
        }
        None
    }

    fn row(s: &Seen) -> String {
        let r = &s.req;
        let mut row = format!(
            "{} {} {} {} -> {}",
            s.clock,
            r.ip.as_deref().unwrap_or("-"),
            r.method,
            short(&r.path, 160),
            r.status
        );
        if let Some(ms) = r.ms {
            row.push_str(&format!(" {ms}ms"));
        }
        if !r.user.is_empty() {
            row.push_str(&format!(" user={}", short(&r.user, 40)));
        }
        if let Some(hit) = s.hit {
            row.push_str(&format!(" [{}]", hit.label));
        }
        if !r.agent.is_empty() {
            row.push_str(&format!(" ua={}", short(&r.agent, 70)));
        }
        if !r.referer.is_empty() {
            row.push_str(&format!(" from-page={}", short(&r.referer, 70)));
        }
        row
    }

    /// What is known about the recent requests, with everything `source`
    /// sent listed separately. This is the material for the investigation.
    /// `related` says whether attack-looking requests are what the incident
    /// is about (a flood or a failed sign-in is not about them).
    pub fn evidence(&self, source: Option<&str>, related: bool, now: Instant) -> String {
        let recent: Vec<&Seen> = {
            let mut v: Vec<&Seen> = self.within(now, Duration::from_secs(10 * 60)).collect();
            v.reverse();
            v
        };
        let mut out = Vec::new();
        let flagged = recent.iter().filter(|s| s.hit.is_some()).count();
        let count = |f: &dyn Fn(&Seen) -> Option<String>| {
            let mut m: HashMap<String, usize> = HashMap::new();
            for s in &recent {
                if let Some(k) = f(s) {
                    *m.entry(k).or_insert(0) += 1;
                }
            }
            m
        };
        out.push(format!(
            "Requests in the last 10 minutes: {} ({flagged} look like attacks). Usual rate: about {:.0} per {} seconds.",
            recent.len(),
            self.baseline,
            FLOOD_WINDOW.as_secs()
        ));
        out.push(format!("Answers: {}", top(count(&|s| Some(s.req.status.to_string())), 8)));
        let senders = count(&|s| s.req.ip.clone());
        if senders.is_empty() {
            out.push(
                "Senders: this log does not name them (add PortalControl's request log to the project to see addresses, users and browsers)."
                    .into(),
            );
        } else {
            let unnamed = recent.iter().filter(|s| s.req.ip.is_none()).count();
            let note = if unnamed > 0 {
                format!("; {unnamed} more requests come from log lines that do not name the sender")
            } else {
                String::new()
            };
            out.push(format!("Senders: {}{note}", top(senders, 6)));
        }
        out.push(format!(
            "Most requested: {}",
            top(count(&|s| Some(short(s.req.path.split('?').next().unwrap_or(""), 60))), 8)
        ));
        // Of the sender the incident is about, when there is one.
        let mine = |s: &Seen| source.is_none_or(|ip| s.req.ip.as_deref() == Some(ip));
        let of = source.map(|ip| format!(" of {ip}")).unwrap_or_default();
        let agents = count(&|s| (!s.req.agent.is_empty() && mine(s)).then(|| short(&s.req.agent, 60)));
        if !agents.is_empty() {
            out.push(format!("Browsers and programs{of}: {}", top(agents, 4)));
        }
        let hosts = count(&|s| (!s.req.host.is_empty()).then(|| short(&s.req.host, 60)));
        if hosts.len() > 1 {
            out.push(format!("Host names asked for: {}", top(hosts, 5)));
        }
        let users = count(&|s| (!s.req.user.is_empty() && mine(s)).then(|| s.req.user.clone()));
        if !users.is_empty() {
            out.push(format!("Accounts signed in on requests{of}: {}", top(users, 6)));
        }

        // The incident is about one sender when the log names it. What other
        // senders did in the same minutes is kept apart, so that the
        // investigation does not blame one for the other.
        let not_refused = |list: &[&&Seen]| {
            let n = list.iter().filter(|s| s.hit.is_some() && (200..300).contains(&s.req.status)).count();
            (n > 0).then(|| {
                format!(
                    "Note: {n} of the attack-looking requests were answered with 2xx, so the application did not refuse them."
                )
            })
        };
        let about: Vec<&&Seen> = match source {
            Some(ip) => recent.iter().filter(|s| s.req.ip.as_deref() == Some(ip)).collect(),
            None => recent.iter().collect(),
        };
        match source {
            Some(ip) => {
                let their_hits = about.iter().filter(|s| s.hit.is_some()).count();
                out.push(format!(
                    "\nThis incident is about {ip}. Everything it sent: {} requests, {their_hits} of them look like attacks (latest {}):",
                    about.len(),
                    about.len().min(40)
                ));
                out.extend(about[about.len().saturating_sub(40)..].iter().map(|s| Self::row(s)));
                out.extend(not_refused(&about));
                if flagged > their_hits {
                    out.push(format!(
                        "Other senders sent {} attack-looking request(s) in the same period. That is separate activity and not part of this incident.",
                        flagged - their_hits
                    ));
                }
            }
            None => {
                let hits: Vec<&&Seen> = recent.iter().filter(|s| s.hit.is_some()).collect();
                if related && !hits.is_empty() {
                    out.push(format!("\nRequests that look like attacks (latest {}):", hits.len().min(25)));
                    out.extend(hits[hits.len().saturating_sub(25)..].iter().map(|s| Self::row(s)));
                    out.extend(not_refused(&hits));
                } else if !hits.is_empty() {
                    out.push(format!(
                        "\nFor context only, not part of this incident: {} other request(s) in this period look like attacks.",
                        hits.len()
                    ));
                }
            }
        }
        let fails = about.iter().filter(|s| failed_login(&s.req)).count();
        let accepted = about.iter().filter(|s| accepted_login(&s.req)).count();
        if fails > 0 {
            let whose = source.map(|ip| format!(" by {ip}")).unwrap_or_default();
            out.push(format!("\nSign-ins{whose}: {fails} failed, {accepted} accepted."));
        }
        let signals: Vec<&(Instant, &'static str, String)> =
            self.signals.iter().filter(|s| now.duration_since(s.0) <= Duration::from_secs(10 * 60)).collect();
        if !signals.is_empty() {
            out.push(format!("\nSecurity messages from the application ({}):", signals.len()));
            out.extend(signals[signals.len().saturating_sub(8)..].iter().map(|s| s.2.clone()));
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(ip: &str, path: &str, status: u16) -> Request {
        Request {
            ip: (!ip.is_empty()).then(|| ip.to_string()),
            method: "GET".into(),
            path: path.into(),
            status,
            ..Default::default()
        }
    }

    fn post(ip: &str, path: &str, status: u16) -> Request {
        Request { method: "POST".into(), ..get(ip, path, status) }
    }

    #[test]
    fn requests_are_read_from_the_log_formats() {
        let r = parse_request(
            r#"PCREQ {"ip":"10.0.0.7","xff":"","m":"POST","p":"/accounts/login/?next=/","s":200,"ms":41,"u":"","ua":"Mozilla/5.0","ref":"http://portal.lan/","host":"portal.lan","len":"58"}"#,
        )
        .unwrap();
        assert_eq!(
            (r.ip.as_deref(), r.method.as_str(), r.path.as_str(), r.status, r.ms),
            (Some("10.0.0.7"), "POST", "/accounts/login/?next=/", 200, Some(41))
        );
        assert_eq!((r.agent.as_str(), r.host.as_str()), ("Mozilla/5.0", "portal.lan"));

        // Behind a proxy on this computer, the forwarded address is the sender.
        let r = parse_request(
            r#"x PCREQ {"ip":"127.0.0.1","xff":"203.0.113.9, 10.0.0.1","m":"GET","p":"/","s":200}"#,
        )
        .unwrap();
        assert_eq!(r.ip.as_deref(), Some("203.0.113.9"));
        assert!(parse_request("PCREQ not json").is_none());

        let r = parse_request(
            r#"203.0.113.9 - - [13/Nov/2026:10:22:01 +0000] "GET /wp-login.php HTTP/1.1" 404 179 "-" "sqlmap/1.7""#,
        )
        .unwrap();
        assert_eq!((r.ip.as_deref(), r.path.as_str(), r.status), (Some("203.0.113.9"), "/wp-login.php", 404));
        assert_eq!((r.agent.as_str(), r.referer.as_str()), ("sqlmap/1.7", ""));

        let r =
            parse_request(r#"INFO:     192.168.1.20:53412 - "GET /api/items?id=3 HTTP/1.1" 200 OK"#).unwrap();
        assert_eq!(
            (r.ip.as_deref(), r.path.as_str(), r.status),
            (Some("192.168.1.20"), "/api/items?id=3", 200)
        );

        // Django's runserver does not print the sender.
        let r = parse_request(r#"[13/Nov/2026 10:22:01] "POST /admin/login/ HTTP/1.1" 302 0"#).unwrap();
        assert_eq!((r.ip, r.method.as_str(), r.status), (None, "POST", 302));

        assert!(parse_request("Applying shop.0003_orders... OK").is_none());
        assert!(parse_request(r#"said "GET well soon" 200 times"#).is_none());
    }

    #[test]
    fn attack_requests_are_recognised_and_ordinary_ones_are_not() {
        let label = |path: &str| classify(&get("", path, 200)).map(|h| (h.label, h.strong));
        assert_eq!(
            label("/search?q=1%27%20UNION%20SELECT%20password%20FROM%20auth_user--"),
            Some(("SQL injection", true))
        );
        assert_eq!(label("/download?file=..%2F..%2F..%2Fetc%2Fpasswd"), Some(("path traversal", true)));
        assert_eq!(
            label("/q?name=%3Cscript%3Ealert(1)%3C/script%3E"),
            Some(("script injection (XSS)", true))
        );
        assert_eq!(label("/ping?host=8.8.8.8;cat+/etc/hosts"), Some(("command injection", true)));
        assert_eq!(label("/.env"), Some(("probe for secret files", false)));
        assert_eq!(label("/wp-login.php"), Some(("probe for other software", false)));
        assert_eq!(classify(&get("", "/index.php", 404)).map(|h| h.strong), Some(false));
        assert_eq!(classify(&get("", "/index.php", 200)), None, "a page the application really serves");
        let scanner = Request { agent: "Mozilla/5.0 (compatible; Nikto/2.5)".into(), ..get("", "/", 200) };
        assert_eq!(classify(&scanner).map(|h| h.label), Some("scanner program"));
        for ordinary in
            ["/", "/orders/12/?page=2&sort=-date", "/static/css/app.css", "/accounts/login/?next=/reports/"]
        {
            assert_eq!(label(ordinary), None, "{ordinary}");
        }
    }

    #[test]
    fn one_clear_attack_request_is_reported_at_once_and_a_scan_escalates() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert!(t.request(get("10.0.0.5", "/orders/", 200), t0, "10:00:00").is_none());
        let a = t
            .request(get("203.0.113.9", "/item?id=1' OR 1=1--", 500), t0 + Duration::from_secs(1), "10:00:01")
            .expect("reported");
        assert_eq!((a.kind, a.critical), (Kind::Attack, false));
        assert!(a.what.contains("SQL injection from 203.0.113.9") && a.what.contains("500"), "{}", a.what);
        assert!(
            a.evidence
                .contains("This incident is about 203.0.113.9. Everything it sent: 1 requests, 1 of them"),
            "{}",
            a.evidence
        );
        assert!(a.evidence.contains("10:00:01 203.0.113.9 GET /item?id=1' OR 1=1-- -> 500 [SQL injection]"));
        assert!(a.evidence.contains("Senders: "));

        // More of the same within the cooldown is quiet, until it is a scan.
        let mut scan = None;
        for i in 0..SCAN_MIN as u64 {
            let at = t0 + Duration::from_secs(2 + i);
            let hit =
                t.request(get("203.0.113.9", &format!("/page{i}/../../etc/passwd"), 404), at, "10:00:02");
            if let Some(a) = hit {
                assert!(scan.is_none(), "only one more alarm");
                scan = Some(a);
            }
        }
        let scan = scan.expect("scan");
        assert!(scan.critical && scan.what.contains("automated scan"), "{}", scan.what);
    }

    #[test]
    fn a_few_probes_are_noise_and_five_are_an_incident() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let paths = ["/.env", "/wp-login.php", "/.git/config", "/phpmyadmin/", "/xmlrpc.php"];
        for (i, p) in paths.iter().enumerate() {
            let alarm = t.request(get("", p, 404), t0 + Duration::from_secs(i as u64), "10:00:00");
            assert_eq!(alarm.is_some(), i == PROBE_MIN - 1, "{p}");
            if let Some(a) = alarm {
                assert!(a.what.starts_with("5 probing requests in the last 5 minutes"), "{}", a.what);
                assert!(a.evidence.contains("this log does not name them"));
            }
        }
    }

    #[test]
    fn failed_sign_ins_and_one_that_then_succeeds() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        // Someone else's ordinary sign-in does not count.
        assert!(t.request(post("10.0.0.5", "/accounts/login/", 302), t0, "c").is_none());
        let mut alarms = Vec::new();
        for i in 0..12u64 {
            alarms.extend(t.request(
                post("10.0.0.66", "/accounts/login/", 200),
                t0 + Duration::from_secs(i),
                "c",
            ));
        }
        assert_eq!(alarms.len(), 1);
        assert_eq!((alarms[0].kind, alarms[0].critical), (Kind::BruteForce, false));
        assert!(
            alarms[0].what.starts_with("8 failed sign-ins at /accounts/login/ from 10.0.0.66"),
            "{}",
            alarms[0].what
        );
        assert!(
            alarms[0].evidence.contains("Sign-ins by 10.0.0.66: 8 failed, 0 accepted."),
            "{}",
            alarms[0].evidence
        );

        // The other person signing in is still nothing; the guesser getting in is critical.
        assert!(t
            .request(post("10.0.0.5", "/accounts/login/", 302), t0 + Duration::from_secs(20), "c")
            .is_none());
        let a = t
            .request(post("10.0.0.66", "/accounts/login/", 302), t0 + Duration::from_secs(21), "c")
            .expect("accepted after failures");
        assert!(
            a.critical && a.what.contains("accepted after 12 failed attempts from 10.0.0.66"),
            "{}",
            a.what
        );
    }

    #[test]
    fn a_flood_is_judged_against_the_usual_rate() {
        // A busy application: 100 requests every 10 seconds is its normal.
        let mut busy = Tracker::default();
        let t0 = Instant::now();
        for i in 0..6000u64 {
            let at = t0 + Duration::from_millis(i * 100);
            assert!(busy.request(get("10.0.0.5", "/api/poll", 200), at, "c").is_none(), "request {i}");
        }
        // Three times its normal is still no flood.
        let later = t0 + Duration::from_secs(601);
        for i in 0..300u64 {
            assert!(busy
                .request(get("10.0.0.5", "/api/poll", 200), later + Duration::from_millis(i * 33), "c")
                .is_none());
        }

        // A quiet one: the same 300 in ten seconds is a flood.
        let mut quiet = Tracker::default();
        for i in 0..30u64 {
            quiet.request(get("10.0.0.5", "/", 200), t0 + Duration::from_secs(i * 20), "c");
        }
        let later = t0 + Duration::from_secs(700);
        let mut alarms = Vec::new();
        for i in 0..300u64 {
            let ip = if i % 10 == 0 { "10.0.0.5" } else { "198.51.100.4" };
            alarms.extend(quiet.request(get(ip, "/", 200), later + Duration::from_millis(i * 33), "c"));
        }
        assert_eq!(alarms.len(), 1);
        assert_eq!((alarms[0].kind, alarms[0].critical), (Kind::Flood, true));
        assert!(alarms[0].what.starts_with("150 requests in 10 seconds"), "{}", alarms[0].what);
        assert!(alarms[0].what.contains("% of them from 198.51.100.4"), "{}", alarms[0].what);
        assert!(alarms[0].evidence.contains("This incident is about 198.51.100.4."));
    }

    #[test]
    fn a_sweep_of_missing_addresses() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut alarms = Vec::new();
        for i in 0..60u64 {
            let at = t0 + Duration::from_millis(i * 500);
            alarms.extend(t.request(get("10.9.9.9", &format!("/admin{i}/"), 404), at, "c"));
        }
        assert_eq!(alarms.len(), 1);
        assert!(
            alarms[0].what.starts_with("40 requests from 10.9.9.9 were answered with 4xx"),
            "{}",
            alarms[0].what
        );
    }

    #[test]
    fn security_messages_of_django() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let host =
            "Invalid HTTP_HOST header: 'evil.example'. You may need to add 'evil.example' to ALLOWED_HOSTS.";
        assert!(t.signal(host, t0).is_none());
        assert!(t.signal(host, t0 + Duration::from_secs(1)).is_none());
        let a = t.signal(host, t0 + Duration::from_secs(2)).expect("third forged host");
        assert!(a.what.contains("a forged Host header 3 time(s)"), "{}", a.what);
        assert!(a.evidence.contains("Security messages from the application (3):"));
        assert!(t
            .signal("django.core.exceptions.SuspiciousFileOperation: The joined path is outside", t0)
            .is_some());
        assert!(t.signal("Not Found: /favicon.ico", t0).is_none());
        assert!(
            DJANGO_MIDDLEWARE.contains("class RequestLogMiddleware")
                && DJANGO_MIDDLEWARE.contains("\"PCREQ \"")
        );
    }
}
