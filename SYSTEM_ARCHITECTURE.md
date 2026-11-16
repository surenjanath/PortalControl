# PortalControl: System Architecture

This file is the source of truth for how PortalControl is built. It is updated with
every feature, refactor and schema change; the rules for that are in
[section 7](#7-the-update-protocol). `README.md` explains how to *use* the program; this
file explains how it *works*.

| | |
| --- | --- |
| Document version | 1.7.0 |
| Application version | 0.12.0 (`Cargo.toml`) |
| Last updated | 2026-11-16 |

## 1. System Overview

**Purpose.** PortalControl is a process supervisor and deployment dashboard for one
machine. It registers project folders ("targets"), runs their start or deploy scripts
through the machine's own shell, keeps long-running services alive, tracks each
folder's Git state, probes application health, records resource use, takes SQLite
backups and runs security scans. Everything is driven from a web dashboard served by
the program itself.

**Primary users.** A developer or administrator who runs several applications (Django,
Node, workers such as qcluster or Celery) on a workstation or a small server and wants
one place to start, stop, deploy and watch them, without Docker, a CI server or a
process manager per project.

**Form.** One executable (`portalcontrol`, about 5.6 MB on Windows). The dashboard,
SQLite and libgit2 are compiled in. It needs no runtime and no other files, and it is
its own installer (3.8); it creates its data folder on first start.

**Core lifecycle.**

1. A person registers a target in the dashboard: a folder, a script inside it, and
   options (run mode, health URL, schedule, environment, Python environment).
2. The target is stored in SQLite.
3. A run is triggered: by hand, on a schedule, by a new commit (auto-deploy), by a
   webhook, by a crash (auto-restart), or at manager start (autostart).
4. The runner starts the script in the target folder through `cmd.exe /C` or
   `/bin/sh -c`, with the target's environment applied.
5. Output is read line by line, broadcast over a WebSocket to open dashboards, and a
   bounded tail is kept.
6. When the process ends, its status, exit code, duration, commit and log tail are
   written to the history table.
7. Background workers keep observing every target (Git every 30 s, health every 15 s,
   metrics every 5 s) and push changes to the dashboard and the tray icon.

**System boundary.**

```
                         one machine, one user account
 ┌──────────────────────────────────────────────────────────────────────────┐
 │                                                                          │
 │  Browser ── HTTP + WebSocket ──┐          Tray icon (Windows, macOS)     │
 │  (dashboard, loopback/LAN/     │                 │ open / stop / status  │
 │   Tailscale only)              ▼                 ▼                       │
 │                    ┌─────────────────────────────────────┐               │
 │  portalcontrol     │  HTTP API + embedded dashboard      │  api/         │
 │  stop / CLI ─────► │  (axum, guards: host, origin, auth) │  auth.rs      │
 │                    ├─────────────────────────────────────┤  net.rs       │
 │                    │  Shared state (AppState)            │  state.rs     │
 │                    ├───────────┬───────────┬─────────────┤               │
 │                    │ runner    │ workers   │ automation  │               │
 │                    │ processes │ git       │ cron        │               │
 │                    │ log pump  │ health    │ auto-deploy │               │
 │                    │           │ metrics   │ auto-restart│               │
 │                    ├───────────┴───────────┴─────────────┤               │
 │                    │  SQLite (portalcontrol.db)  db.rs   │               │
 │                    └──────┬──────────────┬───────────────┘               │
 │                           │              │                               │
 │              spawns       ▼              ▼   reads                       │
 │   ┌───────────────────────────┐   ┌──────────────────────┐               │
 │   │ target scripts and their  │   │ project folders:     │               │
 │   │ process trees (your apps) │   │ files, .git, venvs   │               │
 │   └───────────────────────────┘   └──────────────────────┘               │
 └───────────────────────────────────────────┬──────────────────────────────┘
                                             │ outbound only
                     ┌───────────────────────┴───────────────────────┐
                     │ Git remotes (fetch / fast-forward pull)       │
                     │ Health URLs of the supervised applications    │
                     │ Inbound: deploy webhooks, from allowed hosts  │
                     └───────────────────────────────────────────────┘
```

## 2. Technology Stack

| Layer | Technology | Notes |
| --- | --- | --- |
| Language | Rust, edition 2021 | One binary crate, `portalcontrol` |
| Async runtime | tokio (multi-thread, 2 to 4 workers) | Processes, sockets, timers, signals |
| HTTP and WebSocket | axum 0.8 | REST API under `/api`, live events on `/ws` |
| Frontend | Plain HTML, CSS and JavaScript | No framework, no build step; files in `web/` are embedded with `include_str!` |
| Database | SQLite through rusqlite (`bundled`) | One file, `portalcontrol.db`, in the data folder |
| Git | libgit2 through git2 (`vendored-libgit2`) | No `git` program is run at runtime. HTTPS uses WinHTTP on Windows and vendored OpenSSL elsewhere |
| Metrics | sysinfo | Host and per-process-tree CPU and memory; also lists this machine's network addresses |
| HTTPS | rustls (ring provider) with axum-server | Serves the dashboard's network address over TLS, HTTP/1.1 only. Not used on the local address |
| Scheduling | Built-in cron parser, chrono for local time | `automation.rs` |
| Password hashing | pbkdf2, sha2, getrandom | Salted PBKDF2-HMAC-SHA256 |
| Webhook signatures | hmac (with sha2) | Verifies GitHub / Gitea `sha256=` signatures in constant time (`webhook.rs`) |
| Encryption at rest | ring (ChaCha20-Poly1305) | Seals stored secrets (`secrets.rs`); ring is already linked in as the rustls provider |
| Tray icon | tray-icon and tao | Windows and macOS only; Linux builds have none |
| Windows resources | winresource (build dependency) | Icon, version details and a manifest (DPI awareness, common controls 6) in the exe. The build fails without it, because the About window needs those controls |
| Windows system calls | windows-sys | Attaching to the parent terminal and the native About window (`TaskDialogIndirect`) |
| Login autostart | Registry Run key (winreg), launchd agent, systemd user unit | `autostart.rs` |
| External tools, optional | `bandit`, `pip-audit` | Only for security scans. PortalControl installs them itself into `<data>/tools/venv` when a scan finds them missing or broken (`toolbox.rs`); a copy on PATH is used when it has none. Installing needs Python 3 and PyPI |
| PDF | Built-in writer (`pdf.rs`) | Reports use the standard Helvetica fonts every PDF reader has, so nothing is embedded and no PDF library is linked |
| Deployment | The program installs itself (`install.rs`, 3.8) | Windows build links the C runtime statically (`.cargo/config.toml`). macOS: `scripts/make-macos-app.sh` builds `PortalControl.app` |
| Local AI model, optional | Ollama over plain HTTP (`ollama.rs`) | Explains incidents and investigates suspected attacks. Runs on this machine or a trusted one; nothing is sent to a cloud service |
| Email alerts, optional | Built-in SMTP client (`mail.rs`) with tokio-rustls and webpki-roots | STARTTLS, implicit TLS or a plain local relay |
| Supported systems | Windows x64, macOS, Linux | Built on the system it runs on |

There are no third-party web APIs. Outbound network traffic is Git fetches and health
probes that the user configured, and PyPI when the scanner tools are installed or
updated (`toolbox.rs`), which happens only when a scan needs them or from Settings.

## 3. Core Services & Modules

### 3.1 Repository layout

| Path | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | Dependencies, version, size-optimised release profile |
| `.cargo/config.toml` | Static C runtime on Windows |
| `build.rs` | Embeds the Windows icon and manifest; records the build commit and time for the About page |
| `src/` | The program (see 3.2) |
| `web/` | The dashboard (see 3.3) |
| `assets/` | `icon.ico`, `icon.icns`, `tray.rgba`, `tray-mac.rgba`; all drawn by `scripts/make-icon.py` |
| `.github/workflows/release.yml` | Runs the tests, then builds Windows (x64) and macOS (universal app) on GitHub and publishes release `v<version>` when the version in `Cargo.toml` has no release yet |
| `.github/workflows/ci.yml` | On every push and pull request: `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on Windows and macOS |
| `rustfmt.toml` | Formatting rules (`max_width = 110`) |
| `scripts/` | `install-autostart.ps1` / `.sh` (build, install, register autostart), `make-icon.py`, `make-macos-app.sh` |
| `integrations/django/portalcontrol_requestlog.py` | Request-log middleware for Django projects; embedded in the program and offered for download (3.9) |
| `README.md` | User guide |
| `docs/screenshots/` | Dashboard screenshots and a PDF report preview used by the README, taken from a demo setup |
| `prd.md` | Original product requirements |
| `SYSTEM_ARCHITECTURE.md` | This file |

### 3.2 Backend modules (`src/`)

| Module | Responsibility |
| --- | --- |
| `main.rs` | Command line (`serve`, `stop`, `install`, `install-autostart`, `uninstall-autostart`, `autostart-status`, `reset-password`, `about`), start-up order, the install sequence, repair of the login entry at start, opening and re-trying the network address, shutdown, tray status text, author and build details |
| `install.rs` | Where the program is installed, replacing the installed copy, the "Install?" question of a double-clicked copy, the Start Menu shortcut (see 3.8) |
| `monitor.rs` | The log monitor (added in 0.10.0): rules and learned patterns over every target's output, incidents, the AI stage with its two briefs (fault, attack), alert emails |
| `threat.rs` | Attack detection: reads requests out of log lines, rules per target, the evidence text for the investigation (see 3.9) |
| `ollama.rs` | Client for a local Ollama server: list models, ask for an answer in a JSON schema |
| `mail.rs`, `alerts.rs` | SMTP client; alert settings, throttling and sending |
| `analytics.rs` | Analytics over everything stored (added in 0.12.0): figures per target and for the machine, the reliability score, the findings (see 3.10). Stores nothing |
| `api/analytics.rs` | `GET /api/analytics`: hands the period, the time and the local time zone to `analytics.rs` |
| `api/incidents.rs` | Incidents, applying a suggested fix, monitor and alert settings, traces, live counters, download of the Django middleware |
| `api/repos.rs` | Cloning a repository for a new target and stored access tokens (added in 0.10.0) |
| `api/mod.rs` | The router, the embedded dashboard assets and the helpers every handler shares |
| `api/access.rs` | Sign-in, sessions and the request guards (allowed hosts, same-origin, network gate) |
| `api/views.rs` | Validation of target settings and the JSON view of a target |
| `api/targets.rs` | Target CRUD, run / stop / restart / deploy / rollback, project start and stop, logs and history |
| `api/inspect.rs` | Per-target scans, Git state, health, metrics, processes and reachability |
| `api/webhooks.rs` | Webhook tokens, signing secrets and the inbound endpoint |
| `api/tools.rs` | Activity feed, cron preview, folder inspection |
| `api/commands.rs` | Quick commands per project type |
| `api/backups.rs` | Database snapshots: list, create, delete, restore |
| `api/transfer.rs` | Export and import |
| `api/detect.rs` | Project detection, generated start scripts, folder browsing, companion processes and launchers |
| `api/manager.rs` | System info, the manager's log, start at login, shutdown / restart, network settings |
| `api/ws.rs` | The event WebSocket |
| `api/reports.rs` | Collects target views, runs, scans and 24-hour metrics into a report and returns the PDF |
| `state.rs` | `AppState`: everything kept in memory and shared between modules |
| `db.rs` | Schema, migrations and all SQL. Each call runs on the blocking pool. That pool must not be capped low: on Windows every output pipe of a running process (two per service) holds one of its threads while waiting for output, so the limit is tokio's default of 512 (`MAX_BLOCKING_THREADS` in `main.rs`) |
| `runner.rs` | Starts scripts and one-off commands, pumps their output, enforces timeouts, kills process trees, queues crashed services for restart |
| `pyenv.rs` | Works out and activates a target's Python environment (see 3.6) |
| `git.rs` | Worker that inspects each repository (30 s) and fetches `origin` (5 min); fast-forward pull, rollback, commit log, changed files |
| `health.rs` | Worker that probes each health URL (15 s) with a small built-in HTTP/1.1 client: `LIVE`, `SLOW`, `DOWN`. A process in its first 30 s that is not answering yet is not recorded as DOWN (startup grace); the probe right after a planned stop or restart, and routine probes of a service that is stopped on purpose or not started yet, update the status without counting as an outage. A service whose last run failed has crashed, and that is recorded |
| `metrics.rs` | Worker that samples the host and every process tree (5 s), keeps 15 minutes in memory and one row per target per minute in SQLite for 7 days |
| `automation.rs` | Cron schedules, deploy (pull then run), auto-deploy on new commits, auto-restart with crash-loop detection, deploy webhooks |
| `secrets.rs` | Encryption at rest of environment variables and webhook signing secrets; the `secret.key` file |
| `webhook.rs` | Checks an inbound deploy webhook: signing secret (HMAC or header), branch filter, ping |
| `security.rs` | Runs `bandit` and `pip-audit`, reduces their JSON to counters, stores the report. A missing or broken tool triggers one install through `toolbox.rs` and a retry. A `requirements.txt` that pins every version is audited with `--no-deps --disable-pip`, so nothing is installed or built to resolve it |
| `toolbox.rs` | Self-healing scanner tools: the managed virtualenv in `<data>/tools/venv`, finding a system Python (the `py` launcher first on Windows), install / repair (`--force-reinstall`) / upgrade, status for Settings |
| `pdf.rs` | Minimal PDF 1.4 writer: pages, Helvetica text with exact widths, wrapping, rectangles, lines, circles |
| `report.rs` | The overall and project reports: layout with page breaks and repeating table headers, the dashboard's status rules, the needs-attention rules |
| `backup.rs` | Consistent SQLite snapshots of a target's database (`VACUUM INTO`), restore, pruning |
| `auth.rs` | Optional dashboard password, sessions, sign-in lockout, the control token |
| `net.rs` | Which addresses may be bound and which `Host` values are answered |
| `netcfg.rs` | Network settings of the manager: stored values, what a start will open, loading the certificate, this machine's addresses (see 3.7) |
| `autostart.rs` | Register and remove start at login on the three systems; read which program an entry starts and point it at another one, keeping its options |
| `tray.rs` | Tray or menu-bar icon: status tooltip, menu, runs the user-interface loop on the main thread. Holds the native About window for Windows (`about_window`); macOS uses the system About panel |
| `util.rs` | Time formatting and the manager's own log file |

### 3.3 Frontend (`web/`)

A single page. `index.html` is the shell; scripts are loaded in order and share plain
global functions. Routing is by URL hash (`#/target/3/logs`).

| File | Holds |
| --- | --- |
| `index.html`, `login.html`, `styles.css`, `icon.svg` | Page shell, sign-in page, all styling (dark and light tokens), the icon |
| `js/core/core.js` | DOM helper `h`, icons, the API client, formatters, shared state, target status and actions |
| `js/core/components.js` | Charts, the log viewer, and small pieces (tables, tiles, form fields) |
| `js/core/router.js` | Hash routing, the view lifecycle and the sidebar |
| `js/core/app.js` | Notifications, command palette, data refresh, WebSocket, start-up |
| `js/pages/home.js` | Dashboard and Projects |
| `js/pages/project.js` | Project page (grouped terminals) |
| `js/pages/new.js` | New Target page with project detection |
| `js/pages/lists.js` | Deployments, Security and Activity pages |
| `js/pages/about.js` | About page |
| `js/pages/incidents.js`, `js/pages/flow.js` | Incidents page; live diagram of the monitor pipeline (added in 0.10.0). The diagram has room for five sources and shows the targets that printed the most; the table under it ("Where the lines come from", 0.11.1) lists every target with its lines, share, error lines and patterns since the manager started |
| `js/pages/analytics.js` | Analytics page (0.12.0): summary tiles, findings, trend charts, runs and incidents per day or hour, the weekday-by-hour grid of failures, the sortable table that compares targets, projects, monitor and AI figures (see 3.10) |
| `js/settings/monitor.js` | Settings sections Email Alerts and AI Monitor, with the attack-detection note and middleware download |
| `js/target/target.js` | The target page frame: header, tabs, saving |
| `js/target/config.js` | Configuration tab, one function per section (`CFG.general`, `CFG.git`, ...) |
| `js/target/python.js` | Configuration section "Python Environment" (`CFG.python`) |
| `js/target/network.js` | Configuration section "Network" (`CFG.network`) |
| `js/target/tabs.js` | Deployments, Commands, Logs and Security tabs |
| `js/target/metrics.js` | Metrics tab |
| `js/settings/settings.js` | Settings page frame: side menu, one section at a time (`SET.<name>`) |
| `js/settings/access.js` | Settings section Access: the dashboard password |
| `js/settings/network.js` | Settings section Network |
| `js/settings/general.js` | Settings sections Start at Login, Notifications, Export and Import |
| `js/settings/system.js` | Settings sections Manager, Resources, Manager Log, Danger Zone |
| `js/login.js` | Sign-in form |

A new script must be added in two places: a `<script>` tag in `index.html` and the
`ASSETS` list in `src/api/mod.rs`. A test fails if they disagree.

### 3.4 How the pieces talk to each other

- **Browser to backend.** JSON over HTTP for every action and for the periodic refresh
  (`/api/targets`, `/api/system`). One WebSocket (`/ws`) carries live events: log
  lines, run started and finished, health and Git changes, activity entries.
- **Modules to each other.** All of them hold an `Arc<AppState>`. They read and write
  the shared maps in `AppState`, call `db`, and announce changes with `AppState::emit`,
  which feeds the WebSocket broadcast channel.
- **Workers.** `git`, `health`, `metrics` and `automation` each run one long-lived
  tokio task started from `main.rs`.
- **Manager to supervised processes.** Only through the operating system: spawn, read
  stdout and stderr pipes, kill the process tree. stdin is closed.
- **Command line to a running manager.** `portalcontrol stop` posts to
  `/api/system/shutdown`, authenticated with the control token file in the data folder.
- **Tray to manager.** With the tray on, the user-interface loop owns the main thread
  and the manager runs on a second thread. They exchange three signals: ready, status
  (checked every 3 s, sent when it changes) and done; "Stop" in the menu notifies the
  manager.

### 3.5 Windowed program on Windows

The Windows executable is built for the windows subsystem (`windows_subsystem =
"windows"` in `main.rs`), so no terminal opens when it is started from Explorer, a
shortcut or the login Run key. At start it calls `AttachConsole` for the parent process,
so commands run from a terminal still print there. Consequences that the code handles:

- `open_browser` starts `cmd /C start` with `CREATE_NO_WINDOW`, like every other program
  the manager spawns, so no terminal flashes.
- When the port is already taken and there is no terminal to show the error in, the
  dashboard of the running instance is opened instead.
- `cmd` and PowerShell do not wait for a windowed program, so its output appears after
  the prompt has returned.

### 3.6 Python environment of a target

`portal_targets.python_env` holds one of three things:

| Value | Meaning |
| --- | --- |
| empty | Nothing is activated. The script runs with whatever is on PATH. This is what targets that existed before 0.5.0 have, and targets created through the API without the field |
| `auto` | Use `.venv`, `venv` or `env` in the working directory if one has a Python for this system. Default for targets created on the New Target page |
| a folder | That environment, wherever it is. A relative path is taken from the working directory. A conda environment folder works |

Activation is done by `pyenv::PyEnv::apply` just before the process starts: the
environment's program folder goes to the front of `PATH`, `VIRTUAL_ENV` is set and
`PYTHONHOME` is removed. The target's own environment variables are applied afterwards
and can override this. A named folder with no Python in it stops the run with an error
instead of silently using another Python. Quick commands use the same environment; with
the setting empty they still prefer an environment found in the folder, as before.
With no environment at all, quick commands, generated launchers and the static-site
starter use `python3` from PATH on macOS and Linux (falling back to `python` when only
that exists) and `python` on Windows (`pyenv::system_python`).

### 3.7 Network access

**The manager.** It always listens on `127.0.0.1:<port>` in plain HTTP (the local
door). The tray icon, `portalcontrol stop` and `reset-password` use only that, so they
work whatever else is configured. Optionally it opens a second listener on one private
address of the machine (the network door), in plain HTTP or, with a certificate, HTTPS.

- Settings are one JSON value under the key `network` in the `settings` table
  (`NetSettings`: `address`, `port`, `hosts`, `https`, `cert_file`, `key_file`).
- At start `main.rs` opens the database, reads the settings, combines them with `--bind`
  and `--port` (the command line wins; `netcfg::effective`) and opens the doors. What
  was opened is kept in `AppState::net` (`netcfg::Active`).
- If the network door cannot be opened, the manager still starts and records the
  reason, which the Settings page shows. It then tries again every 5 seconds
  (`NETWORK_RETRY`) and serves the address as soon as it can be bound, updating
  `AppState::net` and writing a log line and an Activity entry. This is what makes a
  start at login work: the manager usually starts before Wi-Fi, a VPN or Tailscale has
  an address. With `--bind` the failure is fatal only when the program was started from
  a terminal; started in the background it re-tries like the stored setting.
- Settings are read only at start. Saving stores them; `GET /api/system/network`
  reports `restart_needed` when a start would open something different from what is
  running. `POST /api/system/restart` stops the manager and, after everything is
  closed, starts the same executable with the same arguments (minus `--detach` and
  `--open`); the new process retries the port for up to 10 seconds.
- Requests through the HTTPS listener carry a marker (`api::ViaTls`), so sign-in
  cookies set there get the `Secure` attribute.
- Host names listed in the settings are accepted by the request guard in addition to
  `localhost` and private IP literals.
- **Network guard.** Every request through the network door first passes
  `api::network_guard` (a layer only on that listener; it needs the peer address, so
  the listener is served with connect info). It refuses, with HTTP 403 and a plain
  sentence, when no password is set, when the time limit has passed, or when the peer
  is outside the network of the bound address (prefix length taken from the interface,
  /24 if unknown) and other networks are not allowed. The two limits live in
  `AppState::net_limits` and are replaced on every save, so they apply without a
  restart; they are not part of `Effective`. The guard records each peer in
  `AppState::clients` (at most 200) and writes an Activity entry when a peer first
  connects or its allowed/refused state changes. The local door is never guarded.

**A target.** `GET /api/targets/{id}/network` reports the port (from the health URL),
whether the start script was written by PortalControl and for which host, and whether a
TCP connection to the port succeeds on `127.0.0.1` and on each private address of the
machine. The dashboard rewrites a generated start script through the existing
`POST /api/fs/starter` with host `127.0.0.1` or `0.0.0.0`. Scripts PortalControl did
not write are never edited. No listener of the manager sits in front of a project.

### 3.8 Installing and updating

A release is one program file, or on macOS one `.app` folder. `install.rs` gives it one
fixed place per user: `%LOCALAPPDATA%\PortalControl\bin\portalcontrol.exe` on Windows,
`/Applications/PortalControl.app` on macOS (`~/Applications` when that folder cannot be
written), `~/.local/bin/portalcontrol` for a bare binary. `PORTALCONTROL_INSTALL_DIR`
names another folder.

- **`portalcontrol install`** (`install_command` in `main.rs`): if a manager answers on
  the local address it is stopped through `/api/system/shutdown` with the control token
  and the port is awaited; the new copy is written next to the destination, the installed
  copy is renamed aside and the new one renamed into place (`install::replace`; a failed
  step puts the old copy back); on Windows it then waits until the old file can be
  deleted, which is when the old process has ended; an existing login entry is pointed at
  the installed copy; the Start Menu shortcut is written (Windows, through
  `WScript.Shell` in a hidden PowerShell); the installed copy is started with
  `serve --detach` plus the given options and the dashboard is opened when it answers.
  Renaming instead of overwriting is what makes it work while the old version runs:
  Windows locks a running program against writing, not against renaming.
- **Double click.** Started with no arguments, in a release build, from a place that is
  not the install place, the program asks whether to install (`install::offer`: a
  message box on Windows, `osascript` on macOS, nothing on Linux). Yes runs the sequence
  above; No runs from where it is; an install error is shown in a dialog.
  `PORTALCONTROL_NO_INSTALL` turns the question off.
- **Login entry repair** (`heal_autostart` in `main.rs`, at every manager start): when
  the entry starts a file that no longer exists, or the running copy is the installed one
  and the entry starts another file, `autostart::repoint` rewrites the program path and
  keeps the options. On macOS only the plist file is rewritten (launchd reads it at the
  next login; reloading would start a second manager). A copy running from macOS app
  translocation (`/AppTranslocation/`) is never registered: `install-autostart` refuses
  there, and repair skips it.
- A background manager on Windows no longer inherits the output handles of the command
  that started it (`keep_output_handles`), so `portalcontrol install | more` and scripts
  that read the output return when the command ends.
- `/api/system` reports `installed` and `install_path`; Settings -> Start at Login shows
  a note when the running copy is not the installed one. The start of every run logs
  `program: <path>`.
- The launchd agent is written with `ProcessType` `Interactive` instead of `Background`,
  which asked macOS to throttle the manager and the services it starts.

There is no uninstall command; the README says what to delete.

### 3.9 Attack detection

`threat.rs` runs inside the monitor's worker (`monitor.rs`), on the same `log` events,
before the error rules.

1. **Parse.** `parse_request` reads a request out of a line: the `PCREQ {json}` line of
   the Django middleware (`ip`, `xff`, `m`, `p`, `s`, `ms`, `u`, `ua`, `ref`, `host`,
   `len`; the forwarded address is used when the direct sender is loopback), the
   common/combined access-log format (gunicorn, uvicorn, nginx) and Django's `runserver`
   line, which has no sender. `PCREQ` lines are not fed to the error rules or the learned
   patterns.
2. **Rules.** One `Tracker` per target keeps the last 3,000 requests of 30 minutes.
   `classify` marks a request (percent-decoded, lower-cased): SQL injection, path
   traversal, script injection, command injection, code-execution probes and scanner
   user agents are "strong"; secret files, other software's paths and foreign page types
   answered 404 are probes. Counted per sender when the log names one, else over all
   requests: one strong request; 5 probes in 5 minutes; 20 marked requests in 5 minutes
   (critical); 8 failed sign-ins in 5 minutes (a POST to a sign-in path answered 200,
   400, 401, 403 or 429); a sign-in accepted (301 to 303) after those (critical); 20
   answers 401; 40 answers 4xx in a minute; 150 requests in 10 seconds that are also 6
   times the smoothed usual rate (critical; flood buckets are not learned). `signal`
   handles Django's own lines: `Invalid HTTP_HOST header` 3 times, `Forbidden (CSRF` 10
   times, `SuspiciousOperation` and relatives once. Each rule has a 15-minute cooldown
   per target.
3. **Report.** An alarm is written to Activity at once (kind `security`) and becomes a
   trigger of kind `attack`, `brute_force` or `flood` with its own severity and an
   **evidence** text (`Tracker::evidence`): totals, answers, senders, most requested
   paths, and, scoped to the sender the incident is about, its browsers, signed-in
   accounts, sign-in counts and its last 40 requests with what was recognised in each.
   Other senders' attack-looking requests are only counted, and named as separate.
4. **Investigate.** With AI on, `handle` uses `SECURITY_PROMPT` and `security_prompt`
   (rule, evidence, offered actions, the last 30 raw lines) with the same answer schema
   as faults, and a 75-second limit instead of 180. The evidence is the incident's
   excerpt and, when log lines are allowed in emails, the email's body (first 45 lines).

Nothing is blocked; PortalControl is not in the request path of a project. The middleware
is `integrations/django/portalcontrol_requestlog.py`, embedded with `include_str!` and
served by `GET /api/monitor/django-middleware`. It prints no request bodies, cookies or
other headers.

### 3.10 Analytics

The Analytics page (`#/analytics`) makes one request, `GET /api/analytics?range=24h|7d|30d`
(7 days when left out), and `analytics.rs` answers it from what is already stored. Nothing
is written and there is no new table.

**What is read.** All `metric_points` rows of the period (one per target per minute),
the runs and incidents created in it, the AI traces grouped by status, the newest
security scan of every target, and the monitor's in-memory line counters
(`monitor::stats()`). Days and hours are local: the handler passes the time zone's offset
from UTC, and the tests pass their own.

**Per target** (`TargetStats`): availability (probes answered), response time (average,
median, 95th percentile, maximum), CPU and memory (average, peak), minutes running,
outages with total and longest downtime (`metrics::outages`), mean time to recover and
mean time between outages, runs (succeeded, failed, average and 95th-percentile duration;
quick commands are left out, as on the Metrics tab), crash restarts (runs triggered by
`AUTO_RESTART`), incidents (all, critical, open), log lines and error lines since start,
and the scan counters.

**Derived figures.**

| Figure | Rule |
| --- | --- |
| Memory trend | Least-squares line through the memory of the minutes running: slope in bytes per hour and r². `mem_growing` when there are at least 120 minutes, r² is 0.6 or more, and the line gained at least 20 MB and a fifth of the average |
| Response anomaly | The last hour's average against the time before it (at least 5 and 30 minutes with a probe): flagged when it is more than three standard deviations above, at least 1.5 times and at least 50 ms more |
| Reliability score | 100 minus: 2 points per percent of missed probes (at most 40); the failed share of runs times 25; 3 per crash restart (at most 15); 2 per incident plus 2 more per critical one (at most 10); 5 per critical issue plus 1 per vulnerable package (at most 10). Every deduction is returned with its reason (`deductions`) and shown as the score's tooltip |
| Timeline | Runs and incidents per local hour (24 hours) or local day (7 and 30 days), empty bars included |
| Failure grid | Failed runs and incidents by local weekday and hour |

**Findings** (`insights`), most serious first: a critical issue in the newest scan; outages
(error under 95 % availability); three or more crash restarts (error from ten); 30 % or
more of at least three runs failed; growing memory; a response anomaly; a fifth or more of
at least 1,000 log lines looking like errors; one target printing four fifths of at least
10,000 lines among three or more targets; one hour of the day holding 40 % or more of at
least ten failures.

**For the machine.** The series adds CPU and memory up over the targets per bucket
(5 minutes for 24 hours, otherwise 30), adds the probes up for availability and averages
the response time; `summary` holds the totals and the average score; `projects` groups the
target figures by project.

**Limits of the answer.** They are the limits of the tables: minute rows are kept 7 days
(a 30-day period has resource and health figures for its last 7 days, and the page says
so), 200 runs per target, the newest 500 incidents and traces. Line counters begin again
when the manager restarts. The page asks again at most once a minute while open, and on
Refresh.

## 4. Data Models & State Management

### 4.1 Database

One SQLite file, `portalcontrol.db`, in the data folder
(`%LOCALAPPDATA%\PortalControl`, `~/Library/Application Support/PortalControl` or
`~/.local/share/portalcontrol`; `--data-dir` overrides it).

**`portal_targets`**: one row per target.

| Column | Type | Meaning |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `name` | TEXT | Shown name, up to 100 characters |
| `working_directory` | TEXT | The project folder |
| `script_name` | TEXT | Script inside that folder |
| `git_branch` | TEXT, default `main` | Branch followed on `origin` |
| `current_git_status` | TEXT | `UP_TO_DATE`, `UPDATES_AVAILABLE` or `DIRTY` |
| `created_at` | DATETIME | UTC |
| `health_url` | TEXT, nullable | Probed every 15 s |
| `run_mode` | TEXT | `TASK` (runs to completion) or `SERVICE` (long-running) |
| `autostart` | INTEGER | Start when the manager starts |
| `timeout_secs` | INTEGER, default 300 | Tasks only |
| `description` | TEXT | |
| `project` | TEXT, default `Default` | Groups targets |
| `env_vars` | TEXT | `.env` format, one `KEY=VALUE` per line; stored sealed (`enc1:…`, see `secrets.rs`) |
| `schedule` | TEXT, nullable | Cron expression |
| `auto_deploy` | INTEGER | Deploy when `origin` has new commits |
| `auto_restart` | INTEGER | Restart a crashed service |
| `webhook_token` | TEXT, nullable | Secret in the deploy webhook URL |
| `webhook_secret` | TEXT, nullable | Optional signing secret a webhook caller must prove (HMAC or header); stored sealed |
| `db_file` | TEXT, nullable | SQLite file inside the folder to back up |
| `backup_on_deploy` | INTEGER | Snapshot before each run |
| `start_order` | INTEGER, default 0 | Position in the project's start sequence |
| `python_env` | TEXT, nullable | See 3.6 (added in 0.5.0) |

**`process_execution_history`**: one row per run or quick command; the newest 200 per
target are kept.

| Column | Meaning |
| --- | --- |
| `id`, `target_id` (FK, cascade) | |
| `triggered_by` | `MANUAL`, `RESTART`, `SCHEDULE`, `GIT_AUTO`, `AUTO_RESTART`, `WEBHOOK`, `ROLLBACK`, `SYSTEM_AUTO` or `COMMAND` |
| `status` | `RUNNING`, `SUCCESS` or `FAILED` |
| `exit_code`, `duration_ms`, `executed_at` | |
| `captured_logs` | Last 2,000 lines, at most 512 KB |
| `commit_sha`, `commit_message` | Commit that was checked out |
| `label` | Name of the quick command, if it was one |

**`security_scan_reports`**: `id`, `target_id` (FK, cascade), `critical_issues_found`,
`vulnerable_packages_found`, `raw_payload_json`, `scanned_at`.

**`settings`**: `key`, `value`. Holds the dashboard password hash and, under the key
`network`, the network settings as JSON (see 3.7).

**`metric_points`**: one row per target per minute, kept 7 days. Primary key
(`target_id`, `t`); `t` is unix seconds at the start of the minute. Columns `cpu`,
`mem`, `mem_max`, `procs`, `latency`, `probes`, `probes_up`.

**`incidents`** and **`ai_traces`** (added in 0.10.0): what the monitor found, and for
each incident the trace of its analysis (trigger, prompt, model, answer, timing). The
columns are in `db.rs` (`Incident`, `Trace`); an incident's `trigger` is one of `failure`,
`crash_loop`, `health_down`, `deploy_failed`, `error_burst`, `new_error`, and since 0.11.0
`attack`, `brute_force`, `flood`. 0.11.0 adds no tables or columns. The `settings` table
also holds the keys `monitor`, `alerts` and `git_tokens`.

**Migrations.** The base tables are created with `CREATE TABLE IF NOT EXISTS`. Columns
added later are listed in `MIGRATIONS` in `db.rs` and applied with
`add_column_if_missing` at every start, so an older database is upgraded in place and
nothing is lost. Every added column is nullable or has a default.

**Other files in the data folder.** `portalcontrol.log` (the manager's own log),
`control.token` (see 5), `backups/<target id>/` (database snapshots).

### 4.2 In-memory state (`AppState`)

Lost on restart and rebuilt by the workers:

| Field | Holds |
| --- | --- |
| `running` | Live runs by slot: target id for the script, negative target id for a quick command. Each has pid, start time, trigger, a cancel handle and the log tail |
| `health`, `health_history` | Latest probe per target and a ring of recent probes |
| `git` | Latest repository inspection per target |
| `metrics` | 15 minutes of 5-second samples for the host, the manager and each tree |
| `activity` | The last 300 notable events |
| `restart_queue`, `restart_log` | Crashed services waiting to restart, and recent restarts for crash-loop detection |
| `auto_deploy_failed` | Commit an auto-deploy last failed on, so it is not retried forever |
| `scans_running`, `starting_projects` | What is in progress |
| `auth` | Password hash, sessions, failed sign-ins, control token |
| `events` | Broadcast channel feeding every WebSocket client |
| `shutdown`, `restart` | Notified to stop the manager; flag to start it again afterwards |
| `net`, `cli_net` | What this manager listens on (replaced when the network address is opened later; read through `AppState::net()`), and the `--bind` / `--port` it was started with |
| Monitor worker (local to its task) | Per target: the log watch (context lines, learned patterns) and the attack `Tracker` (request history, usual rate, cooldowns) |

### 4.3 Browser state

The dashboard keeps `targets`, `sys` and `activity` in globals, refreshed by polling and
by WebSocket events. Only preferences (theme, notification choice) are in
`localStorage`. The session is an HttpOnly, SameSite=Strict cookie.

### 4.4 Process lifecycle

`start_run` validates the folder and script, resolves the Python environment, builds the
command, takes the `running` lock (so a target cannot start twice), spawns, inserts a
`RUNNING` history row, and starts pump tasks for stdout and stderr. A watcher waits for
exit, timeout or cancel, kills the tree if needed, writes the final row and announces
the result. On manager shutdown every tree is killed; nothing is left orphaned.

## 5. Security & Authentication

PortalControl runs shell scripts, so the design keeps it off the internet and limits
what a request can make it run.

- **Listening addresses.** Only loopback, private-LAN and Tailscale (`100.64.0.0/10`)
  addresses are accepted, from the command line and from Settings → Network alike.
  `0.0.0.0` and public addresses are refused (`net.rs`, `netcfg.rs`). The default is
  loopback only. One exception, in Settings only: an address outside those ranges is
  accepted when it is assigned to this machine at the time of saving and the user ticks
  that the network is internal (`NetSettings::internal_confirmed`), because some
  networks use public address space internally. The request guard then also answers
  to that one address. `--bind` stays strict.
- **Opening to the network needs a password.** `PUT /api/system/network` refuses a
  network address while no dashboard password is set, and the password cannot be
  removed while a network address is saved. Independently of how the address was
  configured, including `--bind`, the network guard refuses every request from another
  computer while no password is set.
- **Same network only, optional time limit.** By default the network guard answers only
  peers on the bound address's own network, and access can be given an end time after
  which every peer is refused (see 3.7).
- **Sign-ins are attributed.** Successful and failed sign-ins are written to Activity
  with the peer address, or "on this computer".
- **Response headers.** `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and
  `Referrer-Policy: no-referrer` on every response.
- **HTTPS.** Optional, for the network address only, with a certificate and key the
  user provides as PEM files. Only the file paths are stored. TLS 1.2 and 1.3 through
  rustls. Sign-in cookies issued over HTTPS are marked `Secure`. The local address is
  plain HTTP and never leaves the machine. Without HTTPS, traffic from other computers,
  including the password, is unencrypted; the Settings page says so.
- **Host and Origin checks.** Requests whose `Host` is not `localhost`, a private
  address or a host name the user listed are rejected (DNS rebinding). Requests with a foreign `Origin` are rejected,
  so another web page cannot drive the API.
- **Password, optional.** Stored only as a salted PBKDF2-HMAC-SHA256 hash in
  `settings`. Signing in gives a random 256-bit session token in an HttpOnly cookie;
  sessions live in memory and end when the manager restarts, on sign-out and when the
  password changes. Repeated wrong passwords lock sign-in for a short while. Without a
  password, anyone who can reach the bound address can use it, which is why the default
  is `127.0.0.1`.
- **Control token.** A random token written to `control.token` in the data folder at
  start. `portalcontrol stop` sends it. Being able to read the file proves access to
  this user's files.
- **Password recovery.** `portalcontrol reset-password` clears the hash directly in the
  database; it needs file access and the manager stopped.
- **What can be executed.** Only a script file that exists inside the registered
  folder; names with path traversal or shell metacharacters are rejected. Quick
  commands are a fixed list of program and arguments, run without a shell; nothing typed
  in the dashboard is executed. Generated start scripts contain only the detected
  command.
- **Webhooks.** A deploy webhook needs no session; its unguessable per-target token in
  the URL is its credential, and it can only trigger that target's deploy. It accepts
  POST only (GET is refused, so link previews cannot deploy). An optional signing
  secret makes the URL alone insufficient: GitHub/Gitea HMAC signatures are verified
  in constant time, GitLab and script callers send the secret in a header. Push
  payloads for other branches or tags are acknowledged without deploying
  (`webhook.rs`).
- **Installing scanner tools.** `toolbox.rs` runs `pip install bandit pip-audit` from
  PyPI into PortalControl's own virtualenv, never into the system Python or a project.
  It is triggered by a scan that finds a tool missing or broken, or from Settings, and
  each install is written to Activity. Versions are not pinned: "Update to latest" and
  first installs take the newest releases PyPI serves. Reports leave out environment
  values, webhook tokens and secrets.
- **Sign-in throttling.** Five wrong passwords in a minute lock out that client
  address only; thirty from all addresses together lock out everyone, as a backstop.
- **Secrets.** Target environment variables and webhook signing secrets are encrypted
  in the database (ChaCha20-Poly1305, `secrets.rs`) with a random key in `secret.key`
  next to it (mode 600 on macOS and Linux). A copied or backed-up database file alone
  does not reveal them; someone who can read the whole data folder can. Plaintext from
  older versions is sealed at start, and `secure_delete` zeroes what it replaces. A
  value that cannot be decrypted reads as empty and is not overwritten by an empty
  save; an unreadable webhook secret refuses every delivery. They are masked in the
  dashboard until revealed, and exports contain only variable names unless values are
  explicitly included. Git
  credentials are never stored; libgit2 uses the user's credential helper.
- **Browser hardening.** A Content-Security-Policy allows scripts only from the program
  itself; there are no inline scripts.
- **Limits.** Log lines are capped at 8 KB; tasks are killed at their timeout; stdin is
  closed so a script cannot wait for input forever.

## 6. Current State & Known Limitations

**Working and covered by tests (150 at the time of writing).** Analytics: the memory
trend (steady growth found, an up-and-down pattern and a flat line not), availability,
outages and recovery times, the response anomaly, the score with its deductions and
ceilings, run counting without quick commands, the timeline and failure grid in local
time, the order of findings, and the whole answer built from a database. Installing: the install
place of a program and of an app bundle, replacing a file and a whole folder, leaving the
installed copy alone when the copy fails; pointing a login entry at another program on all
three systems with its options kept. Attack detection: reading the three log formats,
recognising attack requests and leaving ordinary ones alone, each rule with its threshold,
the flood rule against a busy and a quiet baseline, Django's security messages, the
evidence scoped to one sender. Target registration and
validation, running and stopping scripts, services with auto-restart, timeouts, quick
commands, live logs, history, Git state, pull, rollback, deploy, auto-deploy, cron
schedules, webhooks, health probes and history, metrics and 7-day history, security
scan parsing, SQLite backups and restore, projects with start order, project detection
with generated start scripts and worker launchers (qcluster, Celery, Node), password
protection with per-client sign-in lockout, export and import (variable values only on request), login autostart, Python environment selection, validation of network settings, webhook signatures and branch filter, encryption at rest of stored secrets, the health startup grace and the rule that a stopped service is not an outage, recognising pinned requirements, the PDF writer (structure, offsets, measuring, wrapping) and both reports (no secrets, page breaks), recognising broken tool installs.

**Working, checked by hand only.** Network access end to end on Windows (HTTPS on a
LAN address with a self-signed certificate, host names, restart from the dashboard,
password rules) and the per-target Network page with a generated start script. The
dashboard pages, the Windows tray icon and its
status text, the About page, the native About window on Windows, starting with no
terminal window. The tray menu entries were not clicked during development on Windows
except through the `about` command, and printing into an interactive `cmd` or
PowerShell window after `AttachConsole` was not observed directly (output through pipes
and files was).

**Checked by hand on macOS (0.9.0).** The server and dashboard (`--no-tray`): every
page, quick commands with `python3`, webhook POST / GET / signatures / branch filter /
ping against a running manager, export with and without values, upgrading a database
with plaintext variables (sealed at start, still passed to runs), and the health
startup grace with a service that takes 20 s to boot. Scanner tools: a scan with
neither tool installed anywhere installed both and completed; a scan with the managed
`bandit` deliberately broken reinstalled it and completed. Both PDF reports were
downloaded from a running manager, opened with poppler (`pdfinfo`, `pdftoppm`) and
checked page by page. The dashboard's Needs attention panel, report buttons and the
Scanner Tools page were checked in Chrome.

**Checked by hand on Windows (0.11.0), with scratch folders, ports and data.**
`portalcontrol install` into an empty folder; installing again over the running installed
copy (stopped, replaced, started, no leftover files, 4 seconds); "already installed". A
login entry pointing at a deleted download folder was re-pointed at the installed copy at
start, options kept. A network address that could not be bound at start was opened by the
retry 15 seconds later without a restart. A stand-in portal printing attack lines produced
the five expected incidents and Activity entries, and with Ollama (`gemma3:4b`) each was
analysed in 4 to 25 seconds; reports about one sender were accurate, while incidents
without a sender picked up details of the other attacks running in the same seconds.

**Checked by hand on Windows (0.12.0), with a scratch data folder and made-up data.** The
Analytics page in Chrome against a development build: five targets, 50,400 minute rows,
140 runs, 22 incidents. Tiles, findings, the five trend charts, the timeline, the failure
grid, the three bar lists, the target table and its sorting, the project table, the
monitor and AI tiles, all three periods (the 30-day note about the 7 days of minute rows
included), dark and light theme. The request took about half a second in a debug build.
Not looked at: a narrow window, and the page on a machine with real output, so the line
counters were all zero.

**Written but not run (0.11.0).** The install question shown by a double click (release
builds only) on Windows and macOS, the Start Menu shortcut, and everything about installing
on macOS: the app bundle copy to Applications, the `osascript` dialogs, the launchd plist
rewrite and the refusal under app translocation. The Django middleware was not run inside
a Django project; its output format is what the parser tests use.

**Written but not yet run on the real system.** The macOS menu-bar icon, its About
panel and `scripts/make-macos-app.sh`. They use the same code as Windows but have not been built
or started on a Mac.

**Nothing is mocked or stubbed.** Fixed values worth knowing: probe every 15 s, Git
inspection every 30 s, fetch every 5 min, metrics every 5 s, tray status every 3 s,
quick-command timeout 900 s, default port 7878.

**Limitations and technical debt.**

- One machine only. There is no agent for remote hosts and no shared database.
- Analytics are worked out on every request from the stored rows (all minute rows of the
  period are read) and are not cached. The reliability score's weights are fixed in
  `analytics.rs`; they are a judgement, not a standard. An incident has no closing time,
  so there is no time-to-resolve figure.
- One user. A password protects the dashboard, but there are no accounts or roles.
- Health checks of `https://` URLs only test that the port answers.
- HTTPS is for the dashboard's network address only. PortalControl does not serve
  HTTPS for the projects it runs, and has no reverse proxy in front of them.
- Network settings, including a renewed certificate, are picked up only at a restart.
  The certificate's names and expiry date are not shown or checked.
- One network address at a time; no listening on every interface.
- The per-target Network page only tests connections from this machine, so it cannot
  see a firewall that blocks other computers.
- Start at login runs at login, not at boot: after a restart nothing runs until the
  user signs in. There is no system service.
- No uninstall command, and no code signing, so each new download meets SmartScreen or
  Gatekeeper again before it can install itself.
- The network address is re-tried forever, also for a cause that will not go away (a
  missing certificate file).
- Attack detection uses fixed thresholds and blocks nothing. Without the middleware or
  an access log with addresses, per-sender rules count all requests together. Paths that
  contain a double quote are not parsed from access-log lines. Request history is in
  memory and lost at a restart. Small models mix up attacks that overlap in time.
- Security scans cover Python only and do not use the target's Python environment
  setting yet; `pip-audit` reads `requirements.txt`, else an environment inside the
  folder.
- The Python version on the Python Environment page comes from `pyvenv.cfg`, so conda
  environments show none.
- Generated start scripts bake in the Python found in the folder at the time they are
  written; they rely on activation when the environment is elsewhere.
- The Windows executable is not code-signed (SmartScreen may warn) and the macOS app is
  only ad-hoc signed.
- No Linux tray icon.
- The frontend uses global functions across script files; load order matters.
- No automated browser tests.

**Next steps under consideration.** Run the macOS build and fix what it shows; use the
target's Python environment for security scans.

## 7. The Update Protocol

These rules apply every time the system is changed. "Update the architecture file"
means: do all of the following in the same piece of work as the code change, before it
is committed.

1. **Read first.** Read this file before changing the code, and check the plan against
   it. If the code and this file disagree, the code is right: fix the file and say so.
2. **New feature.** Add it where it belongs: a row in 3.2 or 3.3 for a new file, a
   paragraph or subsection in section 3 for new behaviour, and section 1 if it changes
   the lifecycle or the boundary diagram.
3. **Schema change.** Update the table in 4.1 in the same change that adds the entry to
   `MIGRATIONS`. State the default and what existing rows get. New columns must be
   nullable or have a default so older databases upgrade in place.
4. **New dependency or tool.** Add it to section 2 with what it is used for.
5. **Security-relevant change.** Anything that touches binding, authentication, what
   can be executed, or stored secrets is described in section 5.
6. **State and limits.** Move items in section 6 between "working", "not yet run" and
   "limitations" as they change. Remove a limitation only when it is really gone. Add
   any new debt the change introduces.
7. **Facts, not plans.** Describe what the code does now. Planned work goes only under
   "Next steps". Never describe something as working unless it was run; say how it was
   checked when that is not an automated test.
8. **Versions.** Bump the application version in `Cargo.toml` using these rules: patch
   for fixes, minor for new features or schema additions, major for a change that
   breaks existing data or the API. Bump the document version the same way for the size
   of the documentation change. Update the header table at the top.
9. **Version History.** Add one row at the top of the table below for every change,
   with the date, both versions and the notable changes. Never edit old rows except to
   correct a mistake.
10. **Keep the README in step.** User-facing behaviour is also described in
    `README.md`; update both.
11. **Remove what is gone.** When code is deleted, delete its description here too.

### Version History

| Document | Application | Date | Changes |
| --- | --- | --- | --- |
| 1.7.0 | 0.12.0 | 2026-11-16 | **Analytics page** (`analytics.rs`, `api/analytics.rs`, `js/pages/analytics.js`, section 3.10): `GET /api/analytics?range=24h\|7d\|30d` works out, from the minute rows, runs, incidents, AI traces, scans and the monitor's counters, figures per target and for the machine (availability, response time, CPU, memory, outages, time to recover and between outages, runs, crash restarts, incidents, log lines), a memory trend that marks steady growth, a check of the last hour's response time against the time before, a reliability score whose every deduction is named, a timeline and a weekday-by-hour grid of failures in local time, a roll-up per project, and a list of findings. The page shows them with a sortable table that compares the targets. New sidebar entry and `chart` icon; Incidents and Analytics added to the command palette. Nothing is stored and there is no schema change. 8 new tests (150). |
| 1.6.1 | 0.11.1 | 2026-11-16 | **Monitor view names the source of the lines.** The pipeline diagram drew the first five targets of the list, so with more than five targets a target printing millions of lines could be one of the hidden ones while the five shown stood at 0.0 lines/s. It now draws the five that printed the most (running first) and gives the rest a dashed box of their own ("+N OTHER", or "NOT IN THE LIST" for a process whose target the page does not know): before, the moving dots of an undrawn target were drawn on the line of the last box, as if that target had printed them; a table under it lists every target with lines, share, error lines, patterns and current rate, busiest first, and opens the target on a click. `GET /api/monitor/stats` gained `error_lines` per target (`Watch::seen_errors`); counters of deleted targets are dropped; the header says "lines since start" (the count is cumulative, the log view keeps the last 2,000 lines per target). **A companion process is not added twice** (`detect.rs`: `Worker::started_by`, `scripts_starting`): when a script in the project folder already starts the Django-Q cluster, the Celery worker or beat, or the npm process (a non-comment line with the command), `GET /api/fs/inspect` names it in `workers[].started_by`; the New target page then leaves "Also add" unticked and says why, and the Configuration tab and its confirmation warn. Found on a Mac where `start.sh` and the generated `portalcontrol_qcluster.sh` ran two clusters on one SQLite database and printed 17 million lines in an hour. No schema change. |
| 1.6.0 | 0.11.0 | 2026-11-13 | **Installing** (`install.rs`, 3.8): `portalcontrol install` and an "Install?" question on double click put the program in one fixed place per user and replace the installed version, also while it runs; Start Menu shortcut on Windows. **Start at login no longer starts an old version**: installing re-points the login entry, and the installed copy repairs an entry that starts another or a missing file at every start (`autostart::registered_exe`, `repoint`); refused under macOS app translocation; launchd agent `ProcessType` now `Interactive`. **After a restart of the computer** the network address is re-tried every 5 seconds until it can be opened, instead of staying shut until a manual restart (`AppState::net` is now replaceable); `--bind` is fatal only from a terminal. Background managers on Windows no longer hold the starter's output pipe. `/api/system` gained `installed`, `install_path`; the log names the program file at start. **Attack detection** (`threat.rs`, 3.9): requests are read from the Django middleware's `PCREQ` lines, access logs and `runserver` lines; rules for injection, traversal, scanners, probes, scans, password guessing and a guessed password, 4xx sweeps, floods and Django's security messages; immediate Activity entry; incidents of kind `attack`, `brute_force`, `flood` with an evidence text; a security brief for the AI model with a 75-second limit; `GET /api/monitor/django-middleware`; `integrations/django/portalcontrol_requestlog.py`. Monitor counters gained `requests`, `threats`. Removed the outdated limitation "environment variables are stored unencrypted" (they are sealed since 0.9.0). |
| 1.5.0 | 0.10.0 | 2026-11-12 | Written on 2026-11-13 from the code, because 0.10.0 was released without updating this file: clone a repository for a new target with stored access tokens (`api/repos.rs`, `git.rs`); the log monitor with rules, learned patterns, incidents and AI analysis through a local Ollama model (`monitor.rs`, `ollama.rs`, `api/incidents.rs`, tables `incidents` and `ai_traces`); email alerts (`mail.rs`, `alerts.rs`); Incidents page, trace view and live pipeline diagram (`incidents.js`, `flow.js`, `settings/monitor.js`). New dependencies tokio-rustls and webpki-roots; serde_json `preserve_order`. Sections 4 and 5 describe these only in outline. |
| 1.4.0 | 0.9.0 | 2026-11-12 | Webhooks: POST only (GET is refused with 405 so link previews cannot deploy); optional signing secret (new column `portal_targets.webhook_secret`, `POST`/`DELETE /api/targets/{id}/webhook/secret`) checked as a GitHub/Gitea HMAC, GitLab token or `X-Webhook-Secret` header; pushes to other branches, tags and GitHub pings are acknowledged without deploying (`webhook.rs`). Environment variables and webhook secrets encrypted at rest (`secrets.rs`, `secret.key`, ChaCha20-Poly1305; plaintext sealed at start; `secure_delete` on). Export leaves out variable values unless asked (`?env_values=1`) and now includes start order and Python environment. Sign-in lockout per client address with a global backstop. Health: 30 s startup grace, planned stops and restarts not counted as outages. Fix: quick commands, launchers and the static-site starter used `python`, which macOS does not have; they now use `python3` there. Fix: the New Target page showed Windows examples on macOS until system info had loaded. `api.rs` split into `api/` with one file per area (code moved unchanged). CI workflow (clippy, tests on Windows and macOS); releases run the tests first. Clippy clean. New dependencies hmac and ring. Self-healing scanner tools (`toolbox.rs`, Settings > Scanner Tools, `GET /api/system/tools`, `POST /api/system/tools/install`): a scan installs or repairs bandit / pip-audit in `<data>/tools/venv` and retries; pinned `requirements.txt` audited without installing. PDF reports (`pdf.rs`, `report.rs`, `GET /api/reports/overall.pdf`, `GET /api/reports/project/{name}`). Dashboard: Needs attention panel, PDF report buttons, critical issues and vulnerable packages shown apart, uptime on cards, per-project running/failing summary. Health: routine probes of a stopped service no longer count as outages. README screenshots (`docs/screenshots/`). |
| 1.3.0 | 0.8.0 | 2026-11-12 | Stricter network access: a guard on the network listener refuses every peer while no password is set, peers outside the bound address's network (default), and everyone after an optional time limit; both limits apply without a restart. Peers are listed in Settings and logged to Activity, sign-ins carry the peer address, two more response headers. New sign-in page. Fix: networks that use address space outside the private ranges internally could not be configured; such an address is now accepted in Settings when the machine has it and the user confirms the network is internal. Fix: on Windows the dashboard stopped answering ("Loading...", empty lists) once eight services were running, because their output pipes used all 16 threads of the blocking pool and database calls had none left; the cap is now 512. The target list also loads scans and last runs in one query each instead of one per target. |
| 1.2.0 | 0.7.0 | 2026-11-12 | Network settings for the manager (`netcfg.rs`, Settings section, `GET`/`PUT /api/system/network`): address for other computers, port, host names, HTTPS with a user-supplied certificate; stored as JSON in `settings`. The manager now always listens on 127.0.0.1 and optionally on one network address; `--bind` adds the network address instead of replacing loopback, and `stop` always uses the local address. Restart from the dashboard (`POST /api/system/restart`). Password required for network access. Per-target Network page (`GET /api/targets/{id}/network`). New dependencies rustls and axum-server; sysinfo `network` feature. Dashboard scripts grouped into folders (`js/core`, `js/pages`, `js/target`, `js/settings`); the Settings page now has a side menu with one section per address (`#/settings/<section>`), each a `SET.<name>` function. Release workflow on GitHub Actions. |
| 1.1.0 | 0.6.0 | 2026-11-12 | Windows build is a windowed program: no terminal window, attaches to the parent terminal for command output, opens the dashboard when started twice (3.5). Native About window from the tray menu and the new `about` command; macOS uses the system About panel. Author, homepage and tagline in `Cargo.toml`, `/api/system` (`author`, `homepage`, `tagline`) and the About page. New dependency windows-sys; the manifest now requests common controls 6 and is required at build time. |
| 1.0.0 | 0.5.0 | 2026-11-12 | First version of this file. In this release: application icon embedded in the Windows executable and used as the browser-tab icon; tray icon on Windows and menu-bar icon on macOS with status tooltip and menu (`tray.rs`, `--no-tray`); macOS app bundle script; C runtime linked statically on Windows; per-target Python environment (`pyenv.rs`, new column `portal_targets.python_env`, `GET /api/fs/pyenv`); About page with version, commit and build time (`/api/system` gained `commit`, `built_at`, `sqlite`, `libgit2`, `started_at`). |

Earlier history, before this file existed, is in the Git log: 0.4.0 added metric
history, project detection with generated start scripts and the redesigned New Target
page; before that came the split dashboard, login, start order, rollback, Node support,
automation, multi-process projects, quick commands and backups.
