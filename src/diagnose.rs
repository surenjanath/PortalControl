//! Diagnosis by rules: what a known error message means and what to do
//! about it, without an AI model (added in 0.14.0).
//!
//! Every incident gets one. With no model, or when the model fails, it is
//! the incident's explanation; with a model it is given to the model as a
//! hint and kept next to its answer, so the two can be compared.
//!
//! A signature is a list of phrases looked for (lower case) in the log
//! lines, newest first, so the most recent error decides. When nothing
//! matches, the incident's kind gives a general diagnosis.

use crate::monitor::Choice;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Diagnosis {
    /// Short id of the signature (`missing_migrations`) or `kind:<trigger>`.
    pub id: &'static str,
    pub title: &'static str,
    pub cause: String,
    pub steps: Vec<String>,
    /// The log line that matched, if a signature did.
    pub matched: Option<String>,
    /// The offered action that fixes it, if one does (an id from `choices`).
    pub action: Option<String>,
}

struct Signature {
    id: &'static str,
    title: &'static str,
    /// Any of these, in lower case, anywhere in a line.
    needles: &'static [&'static str],
    cause: &'static str,
    steps: &'static [&'static str],
    /// Offered actions that fix it, best first. `rollback:` matches any rollback.
    fixes: &'static [&'static str],
}

const SIGNATURES: &[Signature] = &[
    Signature {
        id: "missing_migrations",
        title: "Database schema is behind the code",
        needles: &[
            "no such table",
            "no such column",
            "unapplied migration",
            "undefinedtable",
            "undefinedcolumn",
            "unknown column",
            "table doesn't exist",
        ],
        cause: "The code expects a database table or column that does not exist yet: migrations have not been applied after the last change.",
        steps: &[
            "Apply the migrations (Commands tab: Apply migrations, or `python manage.py migrate`).",
            "Restart the service so it opens fresh database connections.",
            "If a migration fails, check that the database user may alter tables.",
        ],
        fixes: &["command:migrate"],
    },
    Signature {
        id: "missing_package",
        title: "A required package is not installed",
        needles: &["modulenotfounderror", "no module named", "cannot find module", "importerror: cannot import name"],
        cause: "The program imports a package that is not installed in the Python environment (or node_modules) it runs with. Usually requirements changed and were not installed, or the wrong environment is active.",
        steps: &[
            "Install the requirements (Commands tab: Install requirements, or `pip install -r requirements.txt`).",
            "Check Configuration > Python Environment: the target must use the environment the packages are in.",
            "Run the target again.",
        ],
        fixes: &["command:pip-install"],
    },
    Signature {
        id: "port_in_use",
        title: "The port is already taken",
        needles: &[
            "address already in use",
            "only one usage of each socket address",
            "eaddrinuse",
            "errno 98",
            "winerror 10048",
            "port is already in use",
        ],
        cause: "Another process is already listening on the port this service wants, often an earlier copy that was not stopped or another target set to the same port.",
        steps: &[
            "Find the process holding the port (Network tab of the target, or `netstat -ano` / `lsof -i`).",
            "Stop the old copy, or give one of the two another port.",
            "Start the service again.",
        ],
        fixes: &[],
    },
    Signature {
        id: "permission_denied",
        title: "Permission denied",
        needles: &["permission denied", "access is denied", "eacces", "winerror 5]", "operation not permitted"],
        cause: "The process is not allowed to read, write or run a file or folder it needs, or to open a port below 1024.",
        steps: &[
            "Find the path in the error line and check its owner and permissions.",
            "Check that the file is not open or locked by another program (common on Windows).",
            "Run on a port above 1024, or give the account the right it needs.",
        ],
        fixes: &[],
    },
    Signature {
        id: "dependency_down",
        title: "A service it depends on does not answer",
        needles: &[
            "connection refused",
            "econnrefused",
            "could not connect to server",
            "connection reset by peer",
            "name or service not known",
            "getaddrinfo failed",
            "temporary failure in name resolution",
            "winerror 10061",
            "redis.exceptions.connectionerror",
        ],
        cause: "A database, cache, queue or API the program connects to refused the connection or could not be found. The program itself may be fine.",
        steps: &[
            "Check that the database / Redis / API in the error is running and reachable from this machine.",
            "Check host, port and credentials in the environment variables.",
            "Restart this service once the dependency is back.",
        ],
        fixes: &["restart"],
    },
    Signature {
        id: "out_of_memory",
        title: "Out of memory",
        needles: &["memoryerror", "out of memory", "cannot allocate memory", "heap out of memory", "oom-killer"],
        cause: "The process ran out of memory and stopped, or the operating system ended it.",
        steps: &[
            "Look at the Metrics tab: does memory climb steadily (a leak) or jump (one large job)?",
            "Lower the number of workers, or process data in smaller batches.",
            "Restart to recover now; add memory if the load is real.",
        ],
        fixes: &["restart"],
    },
    Signature {
        id: "code_error",
        title: "The code does not load",
        needles: &["syntaxerror", "indentationerror", "taberror", "unexpected token", "unexpected end of input"],
        cause: "A source file has an error that stops the program from loading. It came with the deployed code, not from the environment.",
        steps: &[
            "Open the file and line named in the error and fix it, or",
            "roll back to the last commit that deployed successfully.",
            "Deploy again once the fix is pushed.",
        ],
        fixes: &["rollback:"],
    },
    Signature {
        id: "missing_setting",
        title: "A setting or environment variable is missing",
        needles: &[
            "improperlyconfigured",
            "secret_key setting must not be empty",
            "environment variable",
            "keyerror: '",
            "missing required",
        ],
        cause: "The program needs a setting that is not provided: an environment variable is missing or empty, or a settings file is incomplete.",
        steps: &[
            "Find the setting named in the error.",
            "Add it under Configuration > Environment Variables (or load the .env file there).",
            "Run the target again.",
        ],
        fixes: &[],
    },
    Signature {
        id: "disallowed_host",
        title: "Request for a host name the site does not accept",
        needles: &["disallowedhost", "invalid http_host header"],
        cause: "Django refused a request because its Host name is not in ALLOWED_HOSTS. This is either a name you use that is missing from the list, or someone probing with a forged Host header.",
        steps: &[
            "If the host name is one your users really type, add it to ALLOWED_HOSTS.",
            "If it is not yours, do nothing: Django refused the request, which is correct. Never add '*'.",
        ],
        fixes: &[],
    },
    Signature {
        id: "disk_full",
        title: "The disk is full",
        needles: &["no space left on device", "errno 28", "not enough space on the disk"],
        cause: "The disk the program writes to has no free space left.",
        steps: &[
            "Free space: old logs, backups (Configuration > Database Backups), caches and temporary files.",
            "Run the target again.",
        ],
        fixes: &[],
    },
    Signature {
        id: "database_locked",
        title: "SQLite database is locked",
        needles: &["database is locked", "database table is locked", "sqlite_busy"],
        cause: "Two processes write to the same SQLite file at once and one gave up waiting, for example the web server and a worker, or two copies of the same worker.",
        steps: &[
            "Check that the worker is not started twice (by a script and by its own target).",
            "Raise the SQLite timeout in the database settings, or move to PostgreSQL for concurrent writers.",
        ],
        fixes: &["restart"],
    },
    Signature {
        id: "tls_error",
        title: "Certificate problem on an outgoing connection",
        needles: &["certificate verify failed", "self signed certificate", "unable to get local issuer"],
        cause: "A TLS connection the program makes was refused because the server's certificate could not be verified.",
        steps: &[
            "Check the address the program connects to and the date and time of this machine.",
            "Install the missing CA certificate, or renew the server's certificate.",
        ],
        fixes: &[],
    },
    Signature {
        id: "missing_program",
        title: "A program or file the script calls is missing",
        needles: &[
            "is not recognized as an internal or external command",
            "command not found",
            "the system cannot find the path specified",
            "the system cannot find the file specified",
            "no such file or directory",
        ],
        cause: "The script calls a program or opens a file that does not exist on this machine or is not on PATH.",
        steps: &[
            "Find the name in the error line.",
            "Install the program, or use its full path in the script.",
            "For Python tools, check Configuration > Python Environment.",
        ],
        fixes: &[],
    },
    Signature {
        id: "missing_static",
        title: "Static files are not collected",
        needles: &["missing staticfiles manifest entry", "staticfiles manifest"],
        cause: "Django cannot find the collected static files, so pages that use them fail. collectstatic was not run after the last change.",
        steps: &["Collect static files (Commands tab: Collect static files).", "Restart the service."],
        fixes: &["command:collectstatic"],
    },
    Signature {
        id: "timeout",
        title: "An operation timed out",
        needles: &["timed out", "timeouterror", "read timeout", "deadline exceeded"],
        cause: "Something the program waited for (a database query, an API call, a worker) did not answer in time.",
        steps: &[
            "Find what timed out in the lines above the error.",
            "Check that service's health, and the response time on the Metrics tab.",
            "Restart if it is stuck; raise the timeout only if the work is really that slow.",
        ],
        fixes: &["restart"],
    },
];

type KindDiagnosis =
    (&'static str, &'static str, &'static str, &'static [&'static str], &'static [&'static str]);

/// The general diagnosis of an incident kind, when no signature matched.
fn by_kind(kind: &str) -> KindDiagnosis {
    match kind {
        "crash_loop" => (
            "kind:crash_loop",
            "Crashes again right after each restart",
            "The service exits soon after every start, so automatic restarts cannot help: something it needs at start-up is wrong (code, settings, a dependency).",
            &[
                "Open the Logs tab and read the first error after a start.",
                "If it began with the latest deployment, roll back.",
                "Fix the cause, then start the service by hand.",
            ],
            &["rollback:"],
        ),
        "health_down" => (
            "kind:health_down",
            "Not answering its health check",
            "The service's health URL does not answer: the process may be down, stuck, or listening on another address or port.",
            &[
                "Check that the process is running (target page).",
                "Open the health URL in a browser on this machine.",
                "Check the last lines of its log for errors; restart if it is stuck.",
            ],
            &["restart"],
        ),
        "deploy_failed" => (
            "kind:deploy_failed",
            "Deployment failed",
            "The deploy script or the pull before it did not finish successfully.",
            &[
                "Open the run on the Deployments tab and read the end of its output.",
                "If the pull was refused, see Configuration > Git Source.",
                "Fix the cause and deploy again, or roll back.",
            ],
            &["rollback:"],
        ),
        "error_burst" => (
            "kind:error_burst",
            "Many errors at once",
            "The program printed far more errors than usual within a minute; a dependency, a bad request pattern or a new bug is likely.",
            &["Read the newest error lines below.", "Check the services this one depends on.", "Restart only if it is stuck."],
            &[],
        ),
        "new_error" => (
            "kind:new_error",
            "A kind of error not seen before",
            "The program printed an error message it has not printed since monitoring began.",
            &["Read the error line below.", "Check whether it began with the latest deployment."],
            &[],
        ),
        "attack" => (
            "kind:attack",
            "Hostile requests",
            "Requests that try to break in (injection, path traversal, probing for secret files) were sent to the application.",
            &[
                "Check in the evidence whether any of them got a 2xx or 500 answer: those may have worked.",
                "Block the sender's address at the firewall or router if it is outside your network.",
                "Fix the code behind any request that got a 500.",
            ],
            &[],
        ),
        "brute_force" => (
            "kind:brute_force",
            "Password guessing",
            "Many sign-in attempts failed from one sender in a short time.",
            &[
                "If a sign-in succeeded after the failures, change that account's password and end its sessions.",
                "Block the sender's address, and add rate limiting or a lockout to the sign-in page.",
            ],
            &[],
        ),
        "flood" => (
            "kind:flood",
            "Flood of requests",
            "The application receives far more requests than usual, which can make it slow or unresponsive.",
            &[
                "Find the sender(s) in the evidence and block them if they are not yours.",
                "Restart if the application stopped answering.",
                "Add rate limiting in front of the application.",
            ],
            &["restart"],
        ),
        _ => (
            "kind:failure",
            "The run failed",
            "The script ended with an error. The last lines of its output show why.",
            &["Read the error lines below.", "Fix the cause and run it again."],
            &[],
        ),
    }
}

/// Pick the offered action a diagnosis names, if any is on offer.
fn pick(fixes: &[&str], offered: &[Choice]) -> Option<String> {
    fixes.iter().find_map(|f| {
        offered
            .iter()
            .find(|c| if f.ends_with(':') { c.id.starts_with(f) } else { c.id == *f })
            .map(|c| c.id.clone())
    })
}

/// Diagnose an incident of `kind` from its log lines (oldest first) and the
/// actions on offer. Security incidents use the kind only: their lines are
/// requests, which name errors on purpose.
pub fn diagnose(kind: &str, lines: &[String], offered: &[Choice]) -> Diagnosis {
    let security = matches!(kind, "attack" | "brute_force" | "flood");
    if !security {
        for line in lines.iter().rev().take(400) {
            let lower = line.to_lowercase();
            if let Some(sig) = SIGNATURES.iter().find(|s| s.needles.iter().any(|n| lower.contains(n))) {
                return Diagnosis {
                    id: sig.id,
                    title: sig.title,
                    cause: sig.cause.to_string(),
                    steps: sig.steps.iter().map(|s| s.to_string()).collect(),
                    matched: Some(line.trim().chars().take(400).collect()),
                    action: pick(sig.fixes, offered),
                };
            }
        }
    }
    let (id, title, cause, steps, fixes) = by_kind(kind);
    Diagnosis {
        id,
        title,
        cause: cause.to_string(),
        steps: steps.iter().map(|s| s.to_string()).collect(),
        matched: None,
        action: pick(fixes, offered),
    }
}

/// The line most worth reading: the newest exception line (`XxxError: ...`),
/// else the newest line that looks like an error.
pub fn key_error(lines: &[String]) -> Option<String> {
    let exception = |l: &str| {
        let t = l.trim_start();
        let head = t.split(':').next().unwrap_or("");
        t.contains(':')
            && !head.contains(' ')
            && (head.ends_with("Error") || head.ends_with("Exception") || head.ends_with("Exit"))
    };
    lines
        .iter()
        .rev()
        .find(|l| exception(l))
        .or_else(|| lines.iter().rev().find(|l| crate::monitor::is_error_line(l)))
        .map(|l| l.trim().chars().take(400).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }
    fn offered(ids: &[&str]) -> Vec<Choice> {
        ids.iter().map(|i| Choice { id: i.to_string(), label: i.to_string() }).collect()
    }

    #[test]
    fn known_errors_are_recognised_and_mapped_to_an_offered_fix() {
        let log = lines(
            "Starting\nTraceback (most recent call last):\n  File \"x.py\"\ndjango.db.utils.OperationalError: no such table: shop_order",
        );
        let d = diagnose("failure", &log, &offered(&["restart", "command:migrate"]));
        assert_eq!(d.id, "missing_migrations");
        assert_eq!(d.action.as_deref(), Some("command:migrate"));
        assert!(d.matched.unwrap().contains("no such table"));

        let d = diagnose(
            "failure",
            &lines("ModuleNotFoundError: No module named 'requests'"),
            &offered(&["restart"]),
        );
        assert_eq!(d.id, "missing_package");
        assert_eq!(d.action, None, "the fix is not on offer, so none is picked");

        let d = diagnose(
            "crash_loop",
            &lines("OSError: [Errno 98] Address already in use"),
            &offered(&["restart", "rollback:abc"]),
        );
        assert_eq!(d.id, "port_in_use");
    }

    #[test]
    fn the_newest_error_decides_and_kinds_have_a_fallback() {
        let log = lines("ModuleNotFoundError: No module named 'x'\nlater: connection refused");
        assert_eq!(diagnose("failure", &log, &[]).id, "dependency_down");
        let d = diagnose("health_down", &lines("all quiet"), &offered(&["restart"]));
        assert_eq!(d.id, "kind:health_down");
        assert_eq!(d.action.as_deref(), Some("restart"));
        let d = diagnose("crash_loop", &[], &offered(&["restart", "rollback:1234abcd"]));
        assert_eq!(d.action.as_deref(), Some("rollback:1234abcd"));
        assert!(!d.steps.is_empty());
    }

    #[test]
    fn requests_in_attack_evidence_are_not_read_as_errors() {
        let log = lines("GET /?q=' no such table 404");
        assert_eq!(diagnose("attack", &log, &[]).id, "kind:attack");
    }

    #[test]
    fn the_key_error_is_the_newest_exception_line() {
        let log = lines("ERROR something odd\nTraceback\nValueError: bad value\n  more\nINFO done");
        assert_eq!(key_error(&log).as_deref(), Some("ValueError: bad value"));
        assert_eq!(key_error(&lines("all fine")), None);
    }
}
