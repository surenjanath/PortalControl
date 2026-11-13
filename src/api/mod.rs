//! Axum REST + WebSocket surface of the control plane.
//!
//! Besides target CRUD and execution it exposes deployments, Git details,
//! metrics, activity, webhooks and manager settings to the dashboard.
//!
//! This file holds the router, the embedded dashboard assets and the helpers
//! every handler shares (`fail`, `ok`, `AppResult`). The handlers live in one
//! file per area; each starts with `use super::*` and is re-exported here.

// Handlers return the error response itself (`Result<Response, Response>`),
// so the large `Err` variant is by design, not a value carried around.
#![allow(clippy::result_large_err)]

use crate::automation::{self, Cron};
use crate::db::{Target, TargetInput};
use crate::state::AppState;
use crate::{auth, autostart, backup, git, health, net, netcfg, pyenv, runner, security, webhook};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

mod access;
mod backups;
mod commands;
mod detect;
mod incidents;
mod inspect;
mod manager;
mod reports;
mod repos;
mod targets;
mod tools;
mod transfer;
mod views;
mod webhooks;
mod ws;

use access::*;
pub use access::{network_guard, ViaTls};
use backups::*;
use commands::*;
use detect::*;
pub(crate) use incidents::command_choices;
use incidents::*;
use inspect::*;
use manager::*;
use reports::*;
pub(crate) use repos::load_git_tokens;
use repos::*;
use targets::*;
use tools::*;
use transfer::*;
use views::*;
use webhooks::*;
use ws::*;

// The dashboard is compiled into the binary: one page shell, one stylesheet
// and a handful of scripts, each small enough to read in one sitting.
const INDEX_HTML: &str = include_str!("../../web/index.html");
const LOGIN_HTML: &str = include_str!("../../web/login.html");
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const ASSETS: &[(&str, &str, &str)] = &[
    ("styles.css", CSS, include_str!("../../web/styles.css")),
    ("icon.svg", "image/svg+xml", include_str!("../../web/icon.svg")),
    ("js/core/core.js", JS, include_str!("../../web/js/core/core.js")),
    ("js/core/components.js", JS, include_str!("../../web/js/core/components.js")),
    ("js/core/router.js", JS, include_str!("../../web/js/core/router.js")),
    ("js/pages/home.js", JS, include_str!("../../web/js/pages/home.js")),
    ("js/pages/project.js", JS, include_str!("../../web/js/pages/project.js")),
    ("js/pages/new.js", JS, include_str!("../../web/js/pages/new.js")),
    ("js/target/target.js", JS, include_str!("../../web/js/target/target.js")),
    ("js/target/config.js", JS, include_str!("../../web/js/target/config.js")),
    ("js/target/python.js", JS, include_str!("../../web/js/target/python.js")),
    ("js/target/network.js", JS, include_str!("../../web/js/target/network.js")),
    ("js/target/tabs.js", JS, include_str!("../../web/js/target/tabs.js")),
    ("js/target/metrics.js", JS, include_str!("../../web/js/target/metrics.js")),
    ("js/settings/settings.js", JS, include_str!("../../web/js/settings/settings.js")),
    ("js/settings/access.js", JS, include_str!("../../web/js/settings/access.js")),
    ("js/settings/network.js", JS, include_str!("../../web/js/settings/network.js")),
    ("js/settings/general.js", JS, include_str!("../../web/js/settings/general.js")),
    ("js/settings/system.js", JS, include_str!("../../web/js/settings/system.js")),
    ("js/pages/lists.js", JS, include_str!("../../web/js/pages/lists.js")),
    ("js/pages/about.js", JS, include_str!("../../web/js/pages/about.js")),
    ("js/pages/flow.js", JS, include_str!("../../web/js/pages/flow.js")),
    ("js/pages/incidents.js", JS, include_str!("../../web/js/pages/incidents.js")),
    ("js/settings/monitor.js", JS, include_str!("../../web/js/settings/monitor.js")),
    ("js/core/app.js", JS, include_str!("../../web/js/core/app.js")),
    ("js/login.js", JS, include_str!("../../web/js/login.js")),
];
/// No inline scripts are allowed; inline `style` attributes are.
const CONTENT_POLICY: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self'; \
     connect-src 'self' ws: wss:; img-src 'self' data:; frame-ancestors 'none'";

type AppResult = Result<Response, Response>;
type Params = Query<HashMap<String, String>>;

fn fail(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn internal(e: String) -> Response {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/targets", get(list_targets).post(create_target))
        .route("/api/targets/{id}", get(get_target).put(update_target).delete(delete_target))
        .route("/api/targets/{id}/run", post(run_target))
        .route("/api/targets/{id}/stop", post(stop_target))
        .route("/api/targets/{id}/restart", post(restart_target))
        .route("/api/targets/{id}/deploy", post(deploy_target))
        .route("/api/targets/{id}/logs", get(target_logs))
        .route("/api/targets/{id}/history", get(history).delete(clear_history))
        .route("/api/targets/{id}/scan", get(latest_scan).post(start_scan))
        .route("/api/targets/{id}/scans", get(list_scans))
        .route("/api/targets/{id}/git/refresh", post(git_refresh))
        .route("/api/targets/{id}/git/pull", post(git_pull))
        .route("/api/targets/{id}/git/log", get(git_log))
        .route("/api/targets/{id}/git/changes", get(git_changes))
        .route("/api/targets/{id}/health/check", post(health_check))
        .route("/api/targets/{id}/health/history", get(health_history))
        .route("/api/targets/{id}/metrics", get(target_metrics))
        .route("/api/targets/{id}/webhook", post(webhook_enable).delete(webhook_disable))
        .route("/api/targets/{id}/webhook/secret", post(webhook_secret_create).delete(webhook_secret_remove))
        .route("/api/webhooks/deploy/{token}", get(webhook_get).post(webhook_deploy))
        .route("/api/targets/{id}/commands", get(list_commands))
        .route("/api/targets/{id}/commands/stop", post(stop_command))
        .route("/api/targets/{id}/commands/{command}", post(run_command))
        .route("/api/targets/{id}/backups", get(list_backups).post(create_backup))
        .route("/api/targets/{id}/backups/{file}", axum::routing::delete(delete_backup))
        .route("/api/targets/{id}/backups/{file}/restore", post(restore_backup))
        .route("/api/export", get(export_config))
        .route("/api/import", post(import_config))
        .route("/api/runs", get(recent_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/activity", get(activity))
        .route("/api/cron", get(cron_preview))
        .route("/api/fs/inspect", get(fs_inspect))
        .route("/api/git/clone", post(clone))
        .route("/api/incidents", get(incidents_list))
        .route("/api/incidents/{id}/status", post(incident_status))
        .route("/api/incidents/{id}/act", post(incident_act))
        .route("/api/settings/monitor", get(monitor_get).put(monitor_put))
        .route("/api/settings/alerts", get(alerts_get).put(alerts_put))
        .route("/api/alerts/test", post(alerts_test))
        .route("/api/ai/models", get(ai_models))
        .route("/api/ai/test", post(ai_test))
        .route("/api/traces/{trace_id}", get(trace_get))
        .route("/api/monitor/stats", get(monitor_stats))
        .route("/api/monitor/django-middleware", get(django_middleware))
        .route("/api/git/tokens", get(tokens_list).put(tokens_put))
        .route("/api/fs/launcher", post(fs_launcher))
        .route("/api/system", get(system))
        .route("/api/system/metrics", get(system_metrics))
        .route("/api/system/log", get(system_log))
        .route("/api/system/autostart", post(autostart_install).delete(autostart_remove))
        .route("/api/system/shutdown", post(shutdown))
        .route("/api/system/restart", post(restart))
        .route("/api/system/network", get(network_get).put(network_put))
        .route("/api/system/tools", get(tools_status))
        .route("/api/reports/overall.pdf", get(report_overall))
        .route("/api/reports/project/{name}", get(report_project))
        .route("/api/system/tools/install", post(tools_install))
        .route("/ws", get(ws_upgrade))
        .route("/assets/{*path}", get(asset))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/password", post(auth_password))
        .route("/api/targets/{id}/rollback", post(rollback_target))
        .route("/api/targets/{id}/metrics/history", get(metrics_history))
        .route("/api/targets/{id}/processes", get(target_processes))
        .route("/api/targets/{id}/network", get(target_network))
        .route("/api/fs/list", get(fs_list))
        .route("/api/fs/port", get(fs_port))
        .route("/api/fs/pyenv", get(fs_pyenv))
        .route("/api/fs/starter", post(fs_starter))
        .route("/api/projects/{name}/start", post(project_start))
        .route("/api/projects/{name}/stop", post(project_stop))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

fn html_page(body: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, CONTENT_POLICY),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn asset(Path(path): Path<String>) -> Response {
    match ASSETS.iter().find(|(name, _, _)| *name == path) {
        // `no-cache`: the browser revalidates, so a new build is picked up at once.
        Some((_, mime, body)) => {
            ([(CONTENT_TYPE, *mime), (CACHE_CONTROL, "no-cache")], *body).into_response()
        }
        None => fail(StatusCode::NOT_FOUND, "no such asset"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_drops_env_values_by_default() {
        let env = "# database\nDB_PASSWORD=hunter2\n\nAPI_KEY = abc=def\nDEBUG=0";
        assert_eq!(without_env_values(env), "# database\nDB_PASSWORD=\n\nAPI_KEY =\nDEBUG=");
    }

    fn input(dir: &std::path::Path, script: &str) -> TargetInput {
        TargetInput {
            name: "  Claims Portal ".into(),
            working_directory: dir.to_string_lossy().into_owned(),
            script_name: script.into(),
            health_url: Some("  ".into()),
            run_mode: Some("service".into()),
            ..Default::default()
        }
    }

    #[test]
    fn input_is_normalised_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy.bat"), "@echo off\n").unwrap();

        let t = clean_input(input(dir.path(), "deploy.bat")).unwrap();
        assert_eq!(t.name, "Claims Portal");
        assert_eq!(t.git_branch.as_deref(), Some("main"));
        assert_eq!(t.run_mode.as_deref(), Some("SERVICE"));
        assert_eq!(t.health_url, None);
        assert_eq!(t.timeout_secs, Some(300));
        assert_eq!(t.project.as_deref(), Some("Default"));
        assert_eq!(t.schedule, None);
        assert_eq!(t.env_vars.as_deref(), Some(""));
        assert_eq!((t.auto_deploy, t.auto_restart), (Some(false), Some(false)));

        assert!(clean_input(input(dir.path(), "missing.bat")).is_err());
        assert!(clean_input(input(dir.path(), "..\\deploy.bat")).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.working_directory = "relative/dir".into();
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.run_mode = Some("DAEMON".into());
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.health_url = Some("ftp://nope".into());
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.timeout_secs = Some(0);
        assert!(clean_input(bad).is_err());

        let mut bad = input(dir.path(), "deploy.bat");
        bad.git_branch = Some("--upload-pack=x".into());
        assert!(clean_input(bad).is_err());
    }

    #[test]
    fn new_fields_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy.bat"), "@echo off\n").unwrap();

        let mut good = input(dir.path(), "deploy.bat");
        good.project = Some("  Insurance  ".into());
        good.schedule = Some(" 0 2 * * * ".into());
        good.env_vars = Some("DEBUG=1\r\nSECRET=abc\r\n".into());
        good.description = Some(" claims portal ".into());
        let t = clean_input(good).unwrap();
        assert_eq!(t.project.as_deref(), Some("Insurance"));
        assert_eq!(t.schedule.as_deref(), Some("0 2 * * *"));
        assert_eq!(t.env_vars.as_deref(), Some("DEBUG=1\nSECRET=abc"));
        assert_eq!(t.description.as_deref(), Some("claims portal"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.schedule = Some("every day".into());
        assert!(clean_input(bad).unwrap_err().contains("schedule"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.env_vars = Some("NOT VALID".into());
        assert!(clean_input(bad).unwrap_err().contains("environment"));

        let mut bad = input(dir.path(), "deploy.bat");
        bad.description = Some("x".repeat(501));
        assert!(clean_input(bad).is_err());
    }

    #[test]
    fn directory_inspection_finds_scripts_and_project_type() {
        let dir = tempfile::tempdir().unwrap();
        let (script, other) =
            if cfg!(windows) { ("deploy.bat", "deploy.sh") } else { ("deploy.sh", "deploy.bat") };
        std::fs::write(dir.path().join(script), "echo hi\n").unwrap();
        std::fs::write(dir.path().join(other), "echo hi\n").unwrap();
        std::fs::write(dir.path().join("manage.py"), "").unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts").join(script), "echo hi\n").unwrap();

        let v = inspect_dir(dir.path());
        assert_eq!(v["exists"], true);
        assert_eq!(v["scripts"], json!([script, format!("scripts/{script}")]));
        assert_eq!(v["detected"], json!(["Django"]));
        assert_eq!(v["is_repo"], false);

        assert_eq!(inspect_dir(&dir.path().join("nope"))["exists"], false);
    }

    #[test]
    fn django_workers_are_detected_and_launchers_created() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // Not a Django project: nothing is suggested.
        std::fs::write(p.join("requirements.txt"), "django-q2==1.7\ncelery==5.4\n").unwrap();
        assert!(detect_workers(p).is_empty());
        assert!(create_launcher(p, "qcluster").is_err());

        std::fs::write(p.join("manage.py"), "").unwrap();
        std::fs::create_dir(p.join("portal")).unwrap();
        std::fs::write(
            p.join("portal").join("settings.py"),
            "INSTALLED_APPS = ['django_q']\nQ_CLUSTER = {'name': 'portal'}\n",
        )
        .unwrap();
        let found = detect_workers(p);
        assert_eq!(found.len(), 1, "celery needs a celery.py: {found:?}");
        assert_eq!(found[0].kind, "qcluster");
        let py = pyenv::system_python(std::env::var_os("PATH"));
        assert_eq!(found[0].command, format!("{py} manage.py qcluster"));
        assert!(found[0].why.contains("portal/settings.py"));

        // Celery app module + beat schedule.
        std::fs::write(p.join("portal").join("celery.py"), "app = Celery('portal')\n").unwrap();
        std::fs::write(
            p.join("portal").join("settings.py"),
            "INSTALLED_APPS = ['django_q']\nCELERY_BEAT_SCHEDULE = {}\n",
        )
        .unwrap();
        let kinds: Vec<&str> = detect_workers(p).iter().map(|w| w.kind).collect();
        assert_eq!(kinds, vec!["qcluster", "celery", "celery_beat"]);
        assert!(detect_workers(p)[1].command.contains("-A portal worker"));

        // The project's own virtualenv is preferred.
        let (bin, exe) = if cfg!(windows) { ("Scripts", "python.exe") } else { ("bin", "python") };
        std::fs::create_dir_all(p.join(".venv").join(bin)).unwrap();
        std::fs::write(p.join(".venv").join(bin).join(exe), "").unwrap();
        assert!(detect_workers(p)[0].command.starts_with(".venv"));

        // Creating a launcher writes it once and never overwrites it.
        let (script, created) = create_launcher(p, "qcluster").unwrap();
        assert!(created);
        assert!(runner::validate_target_paths(&p.to_string_lossy(), &script).is_ok());
        let body = std::fs::read_to_string(p.join(&script)).unwrap();
        assert!(body.contains("manage.py qcluster") && body.contains("Created by PortalControl"));
        std::fs::write(p.join(&script), "edited by hand").unwrap();
        assert_eq!(create_launcher(p, "qcluster").unwrap(), (script.clone(), false));
        assert_eq!(std::fs::read_to_string(p.join(&script)).unwrap(), "edited by hand");
        assert!(create_launcher(p, "bogus").is_err());

        let v = inspect_dir(p);
        assert_eq!(v["workers"][0]["kind"], "qcluster");
        assert_eq!(v["workers"][0]["script_exists"], true);
        assert_eq!(v["workers"][1]["script_exists"], false);
    }

    #[test]
    fn project_types_are_detected_with_start_options() {
        let ids = |p: &std::path::Path| -> (String, Vec<&'static str>) {
            let info = detect_project(p);
            (info.kind.to_string(), info.starters.iter().map(|s| s.id).collect())
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(ids(p), ("unknown".to_string(), vec![]));

        std::fs::write(p.join("index.html"), "<h1>hi</h1>").unwrap();
        assert_eq!(ids(p), ("static".to_string(), vec!["static"]));

        std::fs::write(p.join("requirements.txt"), "fastapi\nuvicorn\n").unwrap();
        std::fs::write(p.join("main.py"), "app = FastAPI()").unwrap();
        assert_eq!(ids(p), ("fastapi".to_string(), vec!["uvicorn"]));
        assert!(
            detect_project(p).starters[0].lines[0].contains("uvicorn main:app --host {host} --port {port}")
        );

        // Django wins, and offers Waitress because it is in the requirements.
        std::fs::write(p.join("manage.py"), "").unwrap();
        std::fs::write(p.join("requirements.txt"), "Django\nwaitress\n").unwrap();
        std::fs::create_dir(p.join("portal")).unwrap();
        std::fs::write(p.join("portal").join("wsgi.py"), "").unwrap();
        std::fs::write(p.join("portal").join("settings.py"), "STATIC_ROOT = BASE_DIR / 'static'\n").unwrap();
        let (kind, starters) = ids(p);
        assert_eq!(kind, "django");
        assert_eq!(starters, vec!["runserver", "waitress", "deploy"]);
        let info = detect_project(p);
        assert!(info.starters[1].lines[0].contains("portal.wsgi:application"));
        assert_eq!(info.starters[2].lines.len(), 3, "pip install, migrate, collectstatic");
        assert_eq!((info.starters[2].run_mode, info.starters[2].default_port), ("TASK", None));

        // A front end next to Django adds its npm start option.
        std::fs::write(p.join("package.json"), r#"{"scripts":{"start":"node server.js"}}"#).unwrap();
        assert_eq!(ids(p).1, vec!["runserver", "waitress", "deploy", "npm-start"]);
    }

    #[test]
    fn start_scripts_are_generated_and_user_files_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("manage.py"), "").unwrap();

        assert!(create_starter(p, "runserver", "127.0.0.1", None).unwrap_err().contains("port"));
        assert!(create_starter(p, "runserver", "8.8.8.8", Some(8000)).is_err());
        assert!(create_starter(p, "nope", "127.0.0.1", Some(8000)).is_err());

        let (script, health) = create_starter(p, "runserver", "127.0.0.1", Some(8123)).unwrap();
        assert_eq!(health.as_deref(), Some("http://127.0.0.1:8123/"));
        assert!(runner::validate_target_paths(&p.to_string_lossy(), &script).is_ok());
        let body = std::fs::read_to_string(p.join(&script)).unwrap();
        assert!(
            body.contains("manage.py runserver 127.0.0.1:8123 --noreload") && body.contains(GENERATED_MARKER)
        );
        assert!(!body.contains("{port}") && !body.contains("{host}"));

        // Changing the port regenerates our own script ...
        create_starter(p, "runserver", "0.0.0.0", Some(9000)).unwrap();
        assert!(std::fs::read_to_string(p.join(&script)).unwrap().contains("0.0.0.0:9000"));
        // ... but a file the user wrote under that name is never overwritten.
        std::fs::write(p.join(&script), "my own script").unwrap();
        assert!(create_starter(p, "runserver", "127.0.0.1", Some(8000))
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(std::fs::read_to_string(p.join(&script)).unwrap(), "my own script");

        // The deploy task has no port and stops at the first failing step.
        let (deploy, health) = create_starter(p, "deploy", "127.0.0.1", None).unwrap();
        assert!(health.is_none());
        let body = std::fs::read_to_string(p.join(deploy)).unwrap();
        assert!(body.contains("migrate --noinput"));

        let v = inspect_dir(p);
        assert_eq!(v["project"]["kind"], "django");
        assert_eq!(v["starters"][0]["script_exists"], true);
        assert_eq!(v["starters"][0]["script_is_ours"], false);
    }

    #[test]
    fn folders_are_listed_and_busy_ports_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir(p.join("beta_portal")).unwrap();
        std::fs::create_dir(p.join("Alpha")).unwrap();
        std::fs::create_dir(p.join(".hidden")).unwrap();
        std::fs::write(p.join("beta_portal").join("manage.py"), "").unwrap();
        std::fs::write(p.join("file.txt"), "").unwrap();
        let roots = || vec!["/".to_string()];
        let v = list_dir(&p.to_string_lossy(), roots());
        let names: Vec<&str> =
            v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Alpha", "beta_portal"]);
        assert_eq!(v["entries"][1]["project"], true);
        assert_eq!(v["entries"][0]["project"], false);
        assert!(v["parent"].is_string() && !v["roots"].as_array().unwrap().is_empty());
        assert!(list_dir("relative/path", roots())["error"].is_string());
        assert!(list_dir("", roots())["dir"].is_string(), "defaults to the home folder");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use(port));
        drop(listener);
        assert!(!port_in_use(port));
    }

    #[test]
    fn node_projects_get_commands_and_processes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert!(detect_commands(p, None).is_empty());
        std::fs::write(
            p.join("package.json"),
            r#"{"name":"portal-ui","scripts":{"build":"vite build","dev":"vite","lint":"eslint .","evil; rm -rf /":"x"}}"#,
        )
        .unwrap();
        let ids: Vec<String> = detect_commands(p, None).iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, vec!["npm-install", "npm-run-build", "npm-run-lint"]);
        std::fs::write(p.join("package-lock.json"), "{}").unwrap();
        let cmds = detect_commands(p, None);
        assert_eq!(cmds[1].id, "npm-ci");
        assert_eq!(cmds[2].shown(), "npm run build");
        assert!(cmds[2].changes && !cmds[3].changes);
        assert_eq!(package_scripts(p), vec!["build", "dev", "lint"]);

        let workers = detect_workers(p);
        assert_eq!(workers.len(), 1);
        assert_eq!((workers[0].kind, workers[0].command.as_str()), ("npm_dev", "npm run dev"));
        let (script, created) = create_launcher(p, "npm_dev").unwrap();
        assert!(created && script.starts_with("portalcontrol_npm_dev."));
        assert!(std::fs::read_to_string(p.join(script)).unwrap().contains("npm run dev"));

        // A Django project with a front end gets both sets.
        std::fs::write(p.join("manage.py"), "").unwrap();
        let ids: Vec<String> = detect_commands(p, None).iter().map(|c| c.id.clone()).collect();
        assert!(ids.contains(&"migrate".to_string()) && ids.contains(&"npm-run-build".to_string()));
    }

    #[test]
    fn public_paths_and_session_cookies() {
        for open in [
            "/assets/styles.css",
            "/assets/js/core/app.js",
            "/api/auth/status",
            "/api/auth/login",
            "/api/webhooks/deploy/abc",
        ] {
            assert!(is_public_path(open), "{open}");
        }
        for closed in [
            "/",
            "/api/targets",
            "/ws",
            "/api/auth/password",
            "/api/auth/logout",
            "/api/system/shutdown",
            "/api/webhooks",
        ] {
            assert!(!is_public_path(closed), "{closed}");
        }
        let cookie = session_cookie("tok", 60, false);
        assert!(
            cookie.contains("pc_session=tok")
                && cookie.contains("HttpOnly")
                && cookie.contains("SameSite=Strict")
        );
        assert!(!cookie.contains("Secure"));
        // Signed in over HTTPS: the cookie never travels over plain HTTP.
        assert!(session_cookie("tok", 60, true).ends_with("; Secure"));

        let state = AppState::new(crate::db::Db::open_in_memory().unwrap(), "127.0.0.1:0".parse().unwrap());
        let mut headers = HeaderMap::new();
        assert!(!is_authenticated(&state, &headers));
        let token = state.auth.create_session();
        headers.insert(COOKIE, format!("x=1; pc_session={token}").parse().unwrap());
        assert!(is_authenticated(&state, &headers));
        state.auth.end_session(&token);
        assert!(!is_authenticated(&state, &headers));
        headers.insert(auth::CONTROL_HEADER, state.auth.control_token.parse().unwrap());
        assert!(is_authenticated(&state, &headers));

        // Every asset the page shell references is embedded.
        for html in [INDEX_HTML, LOGIN_HTML] {
            for part in html.split("\"/assets/").skip(1) {
                let name = part.split('"').next().unwrap();
                assert!(ASSETS.iter().any(|(n, _, _)| *n == name), "{name} is not embedded");
            }
        }
    }

    #[test]
    fn log_tail_starts_on_a_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, "first line\nsecond line\nthird line\n").unwrap();
        assert_eq!(tail_file(&path, 1_000).unwrap(), "first line\nsecond line\nthird line\n");
        assert_eq!(tail_file(&path, 15).unwrap(), "third line\n");
    }
}
