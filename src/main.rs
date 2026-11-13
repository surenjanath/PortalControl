//! PortalControl-Core: native cross-platform automation supervisor.
//!
//! A single binary that serves a local web dashboard, tracks Git state of
//! registered project directories, runs their deployment scripts through the
//! host's native shell, streams the output over WebSockets and runs security
//! scans. See `prd.md` and `README.md`.

// A windowed program on Windows: no terminal opens when it is started from
// Explorer, a shortcut or at login. Run from a terminal, it attaches to that
// terminal instead (see `attach_parent_console`).
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

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
mod netcfg;
mod pdf;
mod pyenv;
mod report;
mod runner;
mod secrets;
mod security;
mod state;
mod toolbox;
#[cfg(any(windows, target_os = "macos"))]
mod tray;
mod util;
mod webhook;

use state::AppState;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use util::log;

pub const DEFAULT_PORT: u16 = 7878;

/// Shown on the About page and in the tray's About window.
pub const TAGLINE: &str = "Start, deploy and watch your projects from one place.";
pub const AUTHOR: &str = env!("CARGO_PKG_AUTHORS");
pub const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");

/// Commit and time of this build, when the build recorded them.
pub fn build_stamp() -> String {
    let built = option_env!("PORTALCONTROL_BUILT")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|t| t.with_timezone(&chrono::Local).format("built %Y-%m-%d %H:%M").to_string());
    let commit = option_env!("PORTALCONTROL_COMMIT").map(|c| format!("commit {c}"));
    [commit, built].into_iter().flatten().collect::<Vec<_>>().join(", ")
}

/// A windowed program has no terminal of its own. When it was started from
/// one, connect to it so `portalcontrol stop`, `--help` and errors are shown.
/// Output that was redirected to a file or a pipe is left alone.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: a plain system call with no pointers.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

const HELP: &str = "\
PortalControl-Core - native cross-platform automation supervisor

USAGE:
    portalcontrol [COMMAND] [OPTIONS]

COMMANDS:
    serve                 Run the manager and web dashboard (default)
    stop                  Stop a running manager (same --port and --data-dir)
    install-autostart     Start PortalControl automatically at login
    uninstall-autostart   Remove the login autostart entry
    autostart-status      Show whether autostart is installed
    reset-password        Remove the dashboard password (stop the manager first)
    about                 Show the version and who made it

OPTIONS:
    --bind <IP>           Also listen on this address of the machine, so other
                          computers can connect. Only private-LAN and Tailscale
                          (100.x.y.z) addresses are accepted. The dashboard
                          always answers on 127.0.0.1 as well.
    --port <PORT>         Port to listen on (default 7878)
                          Both can also be set in the dashboard under
                          Settings > Network; these options win over that.
    --data-dir <DIR>      Where the SQLite database and log file live
    --detach              Run in the background without a console window
    --open                Open the dashboard in the default browser
    --no-tray             Do not show the tray icon (Windows notification
                          area, macOS menu bar)
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
    no_tray: bool,
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
        if self.no_tray {
            out.push("--no-tray".to_string());
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
        no_tray: false,
    };
    let mut command_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} requires a value"));
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
                    v.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| format!("invalid port: {v}"))?,
                );
            }
            "--data-dir" => opts.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--detach" => opts.detach = true,
            "--open" => opts.open = true,
            "--no-tray" => opts.no_tray = true,
            "serve"
            | "stop"
            | "install-autostart"
            | "uninstall-autostart"
            | "autostart-status"
            | "reset-password"
            | "about"
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
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4)
}

/// Upper limit of the blocking thread pool (tokio's own default).
const MAX_BLOCKING_THREADS: usize = 512;

fn main() -> ExitCode {
    #[cfg(windows)]
    attach_parent_console();
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
        "about" => {
            println!("PortalControl {}", env!("CARGO_PKG_VERSION"));
            println!("{TAGLINE}");
            println!("Created by {AUTHOR} ({HOMEPAGE})");
            println!("{}", build_stamp());
            #[cfg(windows)]
            tray::about_and_wait(&format!("http://{}/", opts.addr()));
            Ok(())
        }
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
    let pid = spawn_background(args, false)?;
    println!("PortalControl started in the background (pid {pid}).");
    Ok(())
}

/// Set for a manager that was started by its predecessor's "Restart", which
/// may still be letting go of the port.
const RESTARTED_ENV: &str = "PORTALCONTROL_RESTARTED";

/// Start this executable again as a background process with `args`.
fn spawn_background(args: &[String], restarted: bool) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    if restarted {
        cmd.env(RESTARTED_ENV, "1");
    }
    cmd.args(args.iter().filter(|a| !matches!(a.as_str(), "--detach" | "--open")))
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
    Ok(child.id())
}

/// The port stored under Settings > Network, read straight from the database.
fn saved_port(data_dir: &std::path::Path) -> Option<u16> {
    let db_path = data_dir.join("portalcontrol.db");
    if !db_path.is_file() {
        return None;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let db = db::Db::open(&db_path).ok()?;
    rt.block_on(netcfg::NetSettings::load(&db)).port
}

/// Where a running manager can always be reached from this computer.
fn control_addr(opts: &Opts) -> SocketAddr {
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    netcfg::local_addr(opts.port.or_else(|| saved_port(&data_dir)).unwrap_or(DEFAULT_PORT))
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
    let addr = control_addr(opts);
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
        return Err(format!(
            "PortalControl is running at {addr}. Stop it first (portalcontrol stop), then run reset-password again."
        ));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
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
    let addr = control_addr(opts);
    let url = format!("http://{addr}/api/system/shutdown");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    // Authenticates the request when a dashboard password is set.
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    let token = std::fs::read_to_string(data_dir.join(CONTROL_TOKEN_FILE)).unwrap_or_default();
    let token: String = token.trim().chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let headers =
        if token.is_empty() { String::new() } else { format!("X-PortalControl-Token: {token}\r\n") };
    match rt.block_on(health::http_request_with("POST", &url, Duration::from_secs(5), &headers)) {
        Ok(Some(200)) => {
            println!("PortalControl at {addr} is shutting down.");
            Ok(())
        }
        Ok(code) => Err(format!("unexpected response from {addr}: {code:?}")),
        Err(e) => Err(format!("no manager reachable at {addr}: {e}")),
    }
}

pub(crate) fn open_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/C", "start", "", url]);
        // Without this, cmd would open a terminal of its own for a moment.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
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
    // With a tray icon the user-interface loop owns the main thread and the
    // manager runs beside it; `tray::run` exits the process when it is done.
    #[cfg(any(windows, target_os = "macos"))]
    if !opts.no_tray {
        let config = tray::Config {
            url: format!("http://{}/", netcfg::local_addr(opts.port.unwrap_or(DEFAULT_PORT))),
            serve_args: opts.passthrough_args(),
            data_dir: opts.data_dir.clone().unwrap_or_else(default_data_dir),
        };
        tray::run(config, move |link| {
            let link = Arc::new(link);
            let (ready, status) = (link.clone(), link.clone());
            run_manager(
                opts,
                Hooks {
                    quit: link.quit.clone(),
                    on_ready: Box::new(move |url| ready.ready(url)),
                    on_status: Some(Box::new(move |tip, line| status.status(tip, line))),
                },
            )
        });
    }
    run_manager(
        opts,
        Hooks { quit: Arc::new(tokio::sync::Notify::new()), on_ready: Box::new(|_| {}), on_status: None },
    )
}

/// How the tray icon, when there is one, is tied to the manager.
struct Hooks {
    /// Stops the manager from outside (the tray menu).
    quit: Arc<tokio::sync::Notify>,
    /// Called once the manager is listening, with the dashboard address on
    /// this computer.
    on_ready: Box<dyn FnOnce(String) + Send>,
    /// Receives the hover text and the menu's status line when they change.
    on_status: Option<Box<dyn Fn(String, String) + Send + Sync>>,
}

/// Hover text and one-line summary for the tray icon.
/// `services` is (running, total); `down` names targets whose health check fails.
fn status_text(
    addr: SocketAddr,
    targets: usize,
    services: (usize, usize),
    tasks_running: usize,
    down: &[String],
) -> (String, String) {
    let mut parts = Vec::new();
    if targets == 0 {
        parts.push("No targets yet".to_string());
    } else if services.1 > 0 {
        parts.push(format!("{} of {} services running", services.0, services.1));
    } else if tasks_running == 0 {
        parts.push("Nothing running".to_string());
    }
    if tasks_running > 0 {
        parts.push(format!("{tasks_running} task{} running", if tasks_running == 1 { "" } else { "s" }));
    }
    if !down.is_empty() {
        parts.push(format!("{} down", down.len()));
    }
    let line = parts.join(", ");

    let mut tip = format!("PortalControl {}\n{addr}\n{line}", env!("CARGO_PKG_VERSION"));
    if !down.is_empty() {
        tip.push_str(&format!("\nDown: {}", down.join(", ")));
    }
    // Windows shows at most 127 characters.
    if tip.chars().count() > 120 {
        tip = tip.chars().take(119).collect::<String>() + "\u{2026}";
    }
    (tip, line)
}

async fn tray_status(state: &AppState) -> (String, String) {
    let targets = state.db.list_targets().await.unwrap_or_default();
    let running: Vec<i64> = state.running.lock().await.keys().copied().collect();
    let is_service = |id: &i64| targets.iter().any(|t| t.id == *id && t.run_mode == "SERVICE");
    let total_services = targets.iter().filter(|t| t.run_mode == "SERVICE").count();
    let running_services = running.iter().filter(|id| is_service(id)).count();
    let down: Vec<String> = match state.health.lock() {
        Ok(health) => targets
            .iter()
            .filter(|t| health.get(&t.id).is_some_and(|h| h.status == "DOWN"))
            .map(|t| t.name.clone())
            .collect(),
        Err(_) => Vec::new(),
    };
    status_text(
        state.bind,
        targets.len(),
        (running_services, total_services),
        running.len() - running_services,
        &down,
    )
}

/// The manager itself.
fn run_manager(opts: Opts, hooks: Hooks) -> Result<(), String> {
    let Hooks { quit, on_ready, on_status } = hooks;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create data directory {}: {e}", data_dir.display()))?;
    util::init_log_file(&data_dir.join("portalcontrol.log"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        // No small cap on the blocking pool. On Windows every output pipe of
        // a running process (two per service) holds one of its threads while
        // it waits for output, and database calls use the same pool: with a
        // cap of 16, eight running services left none and the API hung.
        // Threads are only created when needed and idle ones are released.
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("portalcontrol")
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start async runtime: {e}"))?;

    let restart = rt.block_on(async move {
        // The port and the network address may be stored in the database.
        let db_path = data_dir.join("portalcontrol.db");
        let db =
            db::Db::open(&db_path).map_err(|e| format!("cannot open database {}: {e}", db_path.display()))?;
        let saved = netcfg::NetSettings::load(&db).await;
        let plan = netcfg::effective(&saved, opts.bind, opts.port, DEFAULT_PORT);

        // The local door. A manager started by "Restart" waits a moment for
        // its predecessor to let go of the port.
        let addr = netcfg::local_addr(plan.port);
        let attempts = if std::env::var_os(RESTARTED_ENV).is_some() { 40 } else { 1 };
        let mut bound = tokio::net::TcpListener::bind(addr).await;
        for _ in 1..attempts {
            if bound.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            bound = tokio::net::TcpListener::bind(addr).await;
        }
        let listener = bound.map_err(|e| {
            // Started by a double click while it is already running: there is
            // no terminal to show the error in, so show the dashboard instead.
            if !std::io::IsTerminal::is_terminal(&std::io::stderr())
                && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
            {
                open_browser(&format!("http://{addr}/"));
            }
            format!("cannot listen on {addr}: {e} (is PortalControl already running?)")
        })?;

        // The door for other computers, if one is configured. When it cannot
        // be opened the manager still runs, reachable locally, and says why.
        let mut active =
            netcfg::Active { effective: plan.clone(), local: addr, network: None, error: None, subnet: None };
        let mut network_listener = None;
        let mut tls = None;
        if let Some(ip) = plan.network {
            let net_addr = SocketAddr::new(ip, plan.port);
            let opened = (|| {
                let tls =
                    if plan.https { Some(netcfg::load_tls(&plan.cert_file, &plan.key_file)?) } else { None };
                let listener = std::net::TcpListener::bind(net_addr)
                    .map_err(|e| format!("cannot listen on {net_addr}: {e}"))?;
                listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                Ok::<_, String>((listener, tls))
            })();
            match opened {
                Ok((listener, config)) => {
                    active.network = Some(net_addr);
                    if let IpAddr::V4(v4) = ip {
                        let prefix = netcfg::local_addresses()
                            .iter()
                            .find(|a| a.ip == v4.to_string())
                            .map(|a| a.prefix);
                        active.subnet = Some((v4, prefix.unwrap_or(24)));
                    }
                    network_listener = Some(listener);
                    tls = config;
                }
                // Asked for on the command line: failing loudly is expected.
                Err(e) if plan.bind_from_cli => return Err(e),
                Err(e) => active.error = Some(e),
            }
        }

        let state = AppState::new(db, addr);
        let _ = state.net.set(active.clone());
        *state.net_limits.lock().unwrap_or_else(|p| p.into_inner()) = netcfg::Limits::from(&saved);
        let _ = state.cli_net.set((opts.bind, opts.port));
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
        match (active.network_url(), &active.error) {
            (Some(url), _) => log(&format!("open to other computers at {url}")),
            (None, Some(e)) => log(&format!("network access is off: {e}")),
            (None, None) => {}
        }

        // Supervisor behaviour: bring up every target flagged for autostart.
        for target in state.db.list_targets().await.unwrap_or_default() {
            if target.autostart {
                if let Err(e) = runner::start_run(&state, &target, "SYSTEM_AUTO").await {
                    log(&format!("autostart of '{}' failed: {e}", target.name));
                }
            }
        }

        if opts.open {
            open_browser(&active.local_url());
        }
        on_ready(active.local_url());
        if let Some(push) = on_status {
            let st = state.clone();
            tokio::spawn(async move {
                let mut shown = String::new();
                loop {
                    let (tip, line) = tray_status(&st).await;
                    if tip != shown {
                        shown = tip.clone();
                        push(tip, line);
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });
        }

        let app = api::router(state.clone());
        // Everything from other computers passes the network guard first.
        let network_app =
            app.clone().layer(axum::middleware::from_fn_with_state(state.clone(), api::network_guard));
        let network_server = async move {
            let Some(listener) = network_listener else {
                return std::future::pending::<Result<(), String>>().await;
            };
            match tls {
                Some(config) => {
                    // Marked so sign-in cookies are sent back over HTTPS only.
                    let service = network_app
                        .layer(axum::Extension(api::ViaTls))
                        .into_make_service_with_connect_info::<SocketAddr>();
                    axum_server::from_tcp_rustls(
                        listener,
                        axum_server::tls_rustls::RustlsConfig::from_config(config),
                    )
                    .map_err(|e| format!("network server error: {e}"))?
                    .serve(service)
                    .await
                    .map_err(|e| format!("network server error: {e}"))
                }
                None => {
                    let listener = tokio::net::TcpListener::from_std(listener)
                        .map_err(|e| format!("network server error: {e}"))?;
                    axum::serve(listener, network_app.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                        .map_err(|e| format!("network server error: {e}"))
                }
            }
        };
        let outcome = tokio::select! {
            r = axum::serve(listener, app) => r.map_err(|e| format!("server error: {e}")),
            r = network_server => r,
            _ = tokio::signal::ctrl_c() => {
                log("interrupt received, shutting down");
                Ok(())
            }
            _ = quit.notified() => {
                log("stop chosen in the tray menu, shutting down");
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
        outcome.map(|()| state.restart.load(std::sync::atomic::Ordering::SeqCst))
    })?;

    // "Restart" in the dashboard: everything above is closed, start afresh
    // with the same options.
    if restart {
        let pid = spawn_background(&args, true)?;
        log(&format!("restarting as pid {pid}"));
    }
    Ok(())
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
            "install-autostart",
            "--port",
            "9000",
            "--bind",
            "100.101.102.103",
            "--data-dir",
            "/x y",
        ]))
        .unwrap();
        assert_eq!(o.command, "install-autostart");
        assert_eq!(
            o.passthrough_args(),
            args(&["--bind", "100.101.102.103", "--port", "9000", "--data-dir", "/x y"])
        );
        assert!(check_bind(&o).is_ok());

        let o = parse_args(&args(&["serve", "--detach", "--open"])).unwrap();
        assert!(o.detach && o.open && !o.no_tray);

        let o = parse_args(&args(&["--no-tray"])).unwrap();
        assert!(o.no_tray);
        assert_eq!(o.passthrough_args(), args(&["--no-tray"]));
    }

    #[test]
    fn tray_status_text_covers_the_common_cases() {
        let addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let (tip, line) = status_text(addr, 0, (0, 0), 0, &[]);
        assert_eq!(line, "No targets yet");
        assert_eq!(
            tip,
            format!("PortalControl {}\n127.0.0.1:7878\nNo targets yet", env!("CARGO_PKG_VERSION"))
        );

        let (tip, line) = status_text(addr, 5, (3, 4), 1, &["api".to_string()]);
        assert_eq!(line, "3 of 4 services running, 1 task running, 1 down");
        assert!(tip.ends_with("\nDown: api"));

        assert_eq!(status_text(addr, 2, (0, 0), 0, &[]).1, "Nothing running");
        assert_eq!(status_text(addr, 2, (0, 0), 2, &[]).1, "2 tasks running");

        let many: Vec<String> = (0..40).map(|i| format!("service-number-{i}")).collect();
        let (tip, _) = status_text(addr, 40, (40, 40), 0, &many);
        assert_eq!(tip.chars().count(), 120);
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
