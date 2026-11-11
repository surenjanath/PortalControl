//! PortalControl-Core: native cross-platform automation supervisor.
//!
//! A single binary that serves a local web dashboard, tracks Git state of
//! registered project directories, runs their deployment scripts through the
//! host's native shell, streams the output over WebSockets and runs security
//! scans. See `prd.md` and `README.md`.

mod api;
mod auth;
mod automation;
mod autostart;
mod backup;
mod db;
mod git;
mod health;
mod metrics;
mod net;
mod runner;
mod security;
mod state;
mod util;

use state::AppState;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use util::log;

const DEFAULT_PORT: u16 = 7878;

const HELP: &str = "\
PortalControl-Core - native cross-platform automation supervisor

USAGE:
    portalcontrol [COMMAND] [OPTIONS]

COMMANDS:
    serve                 Run the manager and web dashboard (default)
    stop                  Stop a running manager (same --bind/--port)
    install-autostart     Start PortalControl automatically at login
    uninstall-autostart   Remove the login autostart entry
    autostart-status      Show whether autostart is installed
    reset-password        Remove the dashboard password (stop the manager first)

OPTIONS:
    --bind <IP>           Address to listen on (default 127.0.0.1).
                          Only loopback, private-LAN and Tailscale (100.x.y.z)
                          addresses are accepted.
    --port <PORT>         Port to listen on (default 7878)
    --data-dir <DIR>      Where the SQLite database and log file live
    --detach              Run in the background without a console window
    --open                Open the dashboard in the default browser
    -h, --help            Show this help
    -V, --version         Show the version

Options given to install-autostart are remembered for the login launch.
";

struct Opts {
    command: String,
    bind: Option<IpAddr>,
    port: Option<u16>,
    data_dir: Option<PathBuf>,
    detach: bool,
    open: bool,
}

impl Opts {
    fn addr(&self) -> SocketAddr {
        SocketAddr::new(
            self.bind.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            self.port.unwrap_or(DEFAULT_PORT),
        )
    }

    /// The serve options the user explicitly chose, as CLI arguments.
    fn passthrough_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(b) = self.bind {
            out.extend(["--bind".to_string(), b.to_string()]);
        }
        if let Some(p) = self.port {
            out.extend(["--port".to_string(), p.to_string()]);
        }
        if let Some(d) = &self.data_dir {
            out.extend(["--data-dir".to_string(), d.to_string_lossy().into_owned()]);
        }
        out
    }
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        command: "serve".into(),
        bind: None,
        port: None,
        data_dir: None,
        detach: false,
        open: false,
    };
    let mut command_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => opts.command = "help".into(),
            "-V" | "--version" => opts.command = "version".into(),
            "--bind" => {
                let v = value("--bind")?;
                opts.bind = Some(v.parse().map_err(|_| format!("invalid IP address: {v}"))?);
            }
            "--port" => {
                let v = value("--port")?;
                opts.port = Some(
                    v.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(|| format!("invalid port: {v}"))?,
                );
            }
            "--data-dir" => opts.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--detach" => opts.detach = true,
            "--open" => opts.open = true,
            "serve" | "stop" | "install-autostart" | "uninstall-autostart" | "autostart-status"
            | "reset-password"
                if !command_seen =>
            {
                opts.command = arg.clone();
                command_seen = true;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(opts)
}

/// Per-user application data directory for the database and log file.
pub fn default_data_dir() -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Local"))
            .join("PortalControl")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/PortalControl")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("portalcontrol")
    }
}

/// A small fixed pool keeps the manager's idle footprint low.
pub fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 4)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    let result = match opts.command.as_str() {
        "help" => {
            println!("{HELP}");
            Ok(())
        }
        "version" => {
            println!("portalcontrol {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "install-autostart" => check_bind(&opts)
            .and_then(|_| autostart::install(&opts.passthrough_args()))
            .map(|m| println!("{m}")),
        "uninstall-autostart" => autostart::uninstall().map(|m| println!("{m}")),
        "autostart-status" => autostart::status().map(|s| match s {
            Some(entry) => println!("Autostart is installed:\n  {entry}"),
            None => println!("Autostart is not installed."),
        }),
        "stop" => stop_remote(&opts),
        "reset-password" => reset_password(&opts),
        _ => {
            if opts.detach {
                detach(&args)
            } else {
                serve(opts)
            }
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// PRD section 5: never listen on a public interface.
fn check_bind(opts: &Opts) -> Result<(), String> {
    let ip = opts.addr().ip();
    if net::is_private_ip(&ip) {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind to {ip}: PortalControl executes shell scripts and only listens on \
             loopback (127.0.0.1), private LAN or Tailscale (100.x.y.z) addresses"
        ))
    }
}

/// Relaunch this executable as a background process and return immediately.
fn detach(args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args.iter().filter(|a| a.as_str() != "--detach"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| format!("cannot start background process: {e}"))?;
    println!("PortalControl started in the background (pid {}).", child.id());
    Ok(())
}

/// File in the data directory holding the running manager's control token.
/// Being able to read it proves local access to this user's files.
const CONTROL_TOKEN_FILE: &str = "control.token";

/// Remove the dashboard password directly in the database. This needs
/// access to this machine's files, which is the recovery path when the
/// password is forgotten. The manager caches the password, so it must be
/// stopped first.
fn reset_password(opts: &Opts) -> Result<(), String> {
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    let db_path = data_dir.join("portalcontrol.db");
    if !db_path.is_file() {
        return Err(format!("no database found at {}", db_path.display()));
    }
    if std::net::TcpStream::connect_timeout(&opts.addr(), Duration::from_millis(500)).is_ok() {
        return Err(format!(
            "PortalControl is running at {}. Stop it first (portalcontrol stop), then run reset-password again.",
            opts.addr()
        ));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let db = db::Db::open(&db_path).map_err(|e| format!("cannot open database: {e}"))?;
    let had = rt.block_on(db.get_setting(auth::PASSWORD_KEY))?.is_some();
    rt.block_on(db.set_setting(auth::PASSWORD_KEY, None))?;
    println!(
        "{}",
        if had {
            "The dashboard password was removed. Start PortalControl and set a new one under Settings."
        } else {
            "No dashboard password was set."
        }
    );
    Ok(())
}

fn stop_remote(opts: &Opts) -> Result<(), String> {
    let url = format!("http://{}/api/system/shutdown", opts.addr());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    // Authenticates the request when a dashboard password is set.
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    let token = std::fs::read_to_string(data_dir.join(CONTROL_TOKEN_FILE)).unwrap_or_default();
    let token: String = token.trim().chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let headers = if token.is_empty() {
        String::new()
    } else {
        format!("X-PortalControl-Token: {token}\r\n")
    };
    match rt.block_on(health::http_request_with("POST", &url, Duration::from_secs(5), &headers)) {
        Ok(Some(200)) => {
            println!("PortalControl at {} is shutting down.", opts.addr());
            Ok(())
        }
        Ok(code) => Err(format!("unexpected response from {}: {code:?}", opts.addr())),
        Err(e) => Err(format!("no manager reachable at {}: {e}", opts.addr())),
    }
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/C", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn serve(opts: Opts) -> Result<(), String> {
    check_bind(&opts)?;
    let addr = opts.addr();
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create data directory {}: {e}", data_dir.display()))?;
    util::init_log_file(&data_dir.join("portalcontrol.log"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .max_blocking_threads(16)
        .thread_name("portalcontrol")
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start async runtime: {e}"))?;

    rt.block_on(async move {
        // Bind first: if another instance owns the port, fail before touching the DB.
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            format!("cannot listen on {addr}: {e} (is PortalControl already running?)")
        })?;

        let db_path = data_dir.join("portalcontrol.db");
        let db = db::Db::open(&db_path)
            .map_err(|e| format!("cannot open database {}: {e}", db_path.display()))?;
        let state = AppState::new(db, addr);
        let _ = state.data_dir.set(data_dir.clone());
        let _ = state.serve_args.set(opts.passthrough_args());
        if let Some(hash) = state.db.get_setting(auth::PASSWORD_KEY).await? {
            state.auth.set_hash(Some(hash));
        }
        if let Err(e) = std::fs::write(data_dir.join(CONTROL_TOKEN_FILE), &state.auth.control_token) {
            log(&format!("could not write the control token file: {e}"));
        }

        git::init();
        git::spawn_worker(state.clone());
        health::spawn_worker(state.clone());
        metrics::spawn_worker(state.clone());
        automation::spawn_worker(state.clone());

        log(&format!(
            "PortalControl-Core {} listening on http://{addr}  (data: {})",
            env!("CARGO_PKG_VERSION"),
            data_dir.display()
        ));

        // Supervisor behaviour: bring up every target flagged for autostart.
        for target in state.db.list_targets().await.unwrap_or_default() {
            if target.autostart {
                if let Err(e) = runner::start_run(&state, &target, "SYSTEM_AUTO").await {
                    log(&format!("autostart of '{}' failed: {e}", target.name));
                }
            }
        }

        if opts.open {
            open_browser(&format!("http://{addr}/"));
        }

        let app = api::router(state.clone());
        let outcome = tokio::select! {
            r = axum::serve(listener, app) => r.map_err(|e| format!("server error: {e}")),
            _ = tokio::signal::ctrl_c() => {
                log("interrupt received, shutting down");
                Ok(())
            }
            _ = state.shutdown.notified() => {
                log("shutdown requested, shutting down");
                // Let the HTTP response for the shutdown request flush.
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(())
            }
        };

        // Never leave orphaned deployment processes behind.
        runner::stop_all(&state).await;
        log("stopped");
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_to_serve_on_loopback() {
        let o = parse_args(&[]).unwrap();
        assert_eq!(o.command, "serve");
        assert_eq!(o.addr().to_string(), "127.0.0.1:7878");
        assert!(o.passthrough_args().is_empty());
        assert!(check_bind(&o).is_ok());
    }

    #[test]
    fn parses_commands_and_options() {
        let o = parse_args(&args(&[
            "install-autostart", "--port", "9000", "--bind", "100.101.102.103", "--data-dir", "/x y",
        ]))
        .unwrap();
        assert_eq!(o.command, "install-autostart");
        assert_eq!(
            o.passthrough_args(),
            args(&["--bind", "100.101.102.103", "--port", "9000", "--data-dir", "/x y"])
        );
        assert!(check_bind(&o).is_ok());

        let o = parse_args(&args(&["serve", "--detach", "--open"])).unwrap();
        assert!(o.detach && o.open);
    }

    #[test]
    fn rejects_public_binds_and_bad_input() {
        for public in ["0.0.0.0", "8.8.8.8", "::"] {
            let o = parse_args(&args(&["--bind", public])).unwrap();
            assert!(check_bind(&o).is_err(), "{public} must be refused");
        }
        assert!(parse_args(&args(&["--port", "0"])).is_err());
        assert!(parse_args(&args(&["--port", "99999"])).is_err());
        assert!(parse_args(&args(&["--bind", "localhost"])).is_err());
        assert!(parse_args(&args(&["--port"])).is_err());
        assert!(parse_args(&args(&["frobnicate"])).is_err());
    }
}
