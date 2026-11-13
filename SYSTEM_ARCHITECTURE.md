# PortalControl: System Architecture

This file is the source of truth for how PortalControl is built. It is updated with
every feature, refactor and schema change; the rules for that are in
[section 7](#7-the-update-protocol). `README.md` explains how to *use* the program; this
file explains how it *works*.

| | |
| --- | --- |
| Document version | 1.4.0 |
| Application version | 0.9.0 (`Cargo.toml`) |
| Last updated | 2026-11-12 |

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
SQLite and libgit2 are compiled in. It needs no installer, no runtime and no other
files; it creates its data folder on first start.

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
| External tools, optional | `bandit`, `pip-audit` | Only for security scans; found on PATH |
| Deployment | Copy the executable | Windows build links the C runtime statically (`.cargo/config.toml`). macOS: `scripts/make-macos-app.sh` builds `PortalControl.app` |
| Supported systems | Windows x64, macOS, Linux | Built on the system it runs on |

There are no third-party web APIs. The only outbound network traffic is Git fetches and
health probes that the user configured.

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
| `README.md` | User guide |
| `prd.md` | Original product requirements |
| `SYSTEM_ARCHITECTURE.md` | This file |

### 3.2 Backend modules (`src/`)

| Module | Responsibility |
| --- | --- |
| `main.rs` | Command line (`serve`, `stop`, `install-autostart`, `uninstall-autostart`, `autostart-status`, `reset-password`, `about`), start-up order, shutdown, tray status text, author and build details |
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
| `state.rs` | `AppState`: everything kept in memory and shared between modules |
| `db.rs` | Schema, migrations and all SQL. Each call runs on the blocking pool. That pool must not be capped low: on Windows every output pipe of a running process (two per service) holds one of its threads while waiting for output, so the limit is tokio's default of 512 (`MAX_BLOCKING_THREADS` in `main.rs`) |
| `runner.rs` | Starts scripts and one-off commands, pumps their output, enforces timeouts, kills process trees, queues crashed services for restart |
| `pyenv.rs` | Works out and activates a target's Python environment (see 3.6) |
| `git.rs` | Worker that inspects each repository (30 s) and fetches `origin` (5 min); fast-forward pull, rollback, commit log, changed files |
| `health.rs` | Worker that probes each health URL (15 s) with a small built-in HTTP/1.1 client: `LIVE`, `SLOW`, `DOWN`. A process in its first 30 s that is not answering yet is not recorded as DOWN (startup grace), and the probe right after a planned stop or restart updates the status without counting as an outage |
| `metrics.rs` | Worker that samples the host and every process tree (5 s), keeps 15 minutes in memory and one row per target per minute in SQLite for 7 days |
| `automation.rs` | Cron schedules, deploy (pull then run), auto-deploy on new commits, auto-restart with crash-loop detection, deploy webhooks |
| `secrets.rs` | Encryption at rest of environment variables and webhook signing secrets; the `secret.key` file |
| `webhook.rs` | Checks an inbound deploy webhook: signing secret (HMAC or header), branch filter, ping |
| `security.rs` | Runs `bandit` and `pip-audit`, reduces their JSON to counters, stores the report |
| `backup.rs` | Consistent SQLite snapshots of a target's database (`VACUUM INTO`), restore, pruning |
| `auth.rs` | Optional dashboard password, sessions, sign-in lockout, the control token |
| `net.rs` | Which addresses may be bound and which `Host` values are answered |
| `netcfg.rs` | Network settings of the manager: stored values, what a start will open, loading the certificate, this machine's addresses (see 3.7) |
| `autostart.rs` | Register and remove start at login on the three systems |
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
  reason, which the Settings page shows. With `--bind` the failure is fatal, as before.
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
| `net`, `cli_net` | What this manager listens on, and the `--bind` / `--port` it was started with |

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

**Working and covered by tests (98 at the time of writing).** Target registration and
validation, running and stopping scripts, services with auto-restart, timeouts, quick
commands, live logs, history, Git state, pull, rollback, deploy, auto-deploy, cron
schedules, webhooks, health probes and history, metrics and 7-day history, security
scan parsing, SQLite backups and restore, projects with start order, project detection
with generated start scripts and worker launchers (qcluster, Celery, Node), password
protection with per-client sign-in lockout, export and import (variable values only on request), login autostart, Python environment selection, validation of network settings, webhook signatures and branch filter, encryption at rest of stored secrets, the health startup grace.

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
startup grace with a service that takes 20 s to boot.

**Written but not yet run on the real system.** The macOS menu-bar icon, its About
panel and `scripts/make-macos-app.sh`. They use the same code as Windows but have not been built
or started on a Mac.

**Nothing is mocked or stubbed.** Fixed values worth knowing: probe every 15 s, Git
inspection every 30 s, fetch every 5 min, metrics every 5 s, tray status every 3 s,
quick-command timeout 900 s, default port 7878.

**Limitations and technical debt.**

- One machine only. There is no agent for remote hosts and no shared database.
- One user. A password protects the dashboard, but there are no accounts or roles.
- Health checks of `https://` URLs only test that the port answers.
- HTTPS is for the dashboard's network address only. PortalControl does not serve
  HTTPS for the projects it runs, and has no reverse proxy in front of them.
- Network settings, including a renewed certificate, are picked up only at a restart.
  The certificate's names and expiry date are not shown or checked.
- One network address at a time; no listening on every interface.
- The per-target Network page only tests connections from this machine, so it cannot
  see a firewall that blocks other computers.
- Environment variables are stored unencrypted.
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
| 1.4.0 | 0.9.0 | 2026-11-12 | Webhooks: POST only (GET is refused with 405 so link previews cannot deploy); optional signing secret (new column `portal_targets.webhook_secret`, `POST`/`DELETE /api/targets/{id}/webhook/secret`) checked as a GitHub/Gitea HMAC, GitLab token or `X-Webhook-Secret` header; pushes to other branches, tags and GitHub pings are acknowledged without deploying (`webhook.rs`). Environment variables and webhook secrets encrypted at rest (`secrets.rs`, `secret.key`, ChaCha20-Poly1305; plaintext sealed at start; `secure_delete` on). Export leaves out variable values unless asked (`?env_values=1`) and now includes start order and Python environment. Sign-in lockout per client address with a global backstop. Health: 30 s startup grace, planned stops and restarts not counted as outages. Fix: quick commands, launchers and the static-site starter used `python`, which macOS does not have; they now use `python3` there. Fix: the New Target page showed Windows examples on macOS until system info had loaded. `api.rs` split into `api/` with one file per area (code moved unchanged). CI workflow (clippy, tests on Windows and macOS); releases run the tests first. Clippy clean. New dependencies hmac and ring. |
| 1.3.0 | 0.8.0 | 2026-11-12 | Stricter network access: a guard on the network listener refuses every peer while no password is set, peers outside the bound address's network (default), and everyone after an optional time limit; both limits apply without a restart. Peers are listed in Settings and logged to Activity, sign-ins carry the peer address, two more response headers. New sign-in page. Fix: networks that use address space outside the private ranges internally could not be configured; such an address is now accepted in Settings when the machine has it and the user confirms the network is internal. Fix: on Windows the dashboard stopped answering ("Loading...", empty lists) once eight services were running, because their output pipes used all 16 threads of the blocking pool and database calls had none left; the cap is now 512. The target list also loads scans and last runs in one query each instead of one per target. |
| 1.2.0 | 0.7.0 | 2026-11-12 | Network settings for the manager (`netcfg.rs`, Settings section, `GET`/`PUT /api/system/network`): address for other computers, port, host names, HTTPS with a user-supplied certificate; stored as JSON in `settings`. The manager now always listens on 127.0.0.1 and optionally on one network address; `--bind` adds the network address instead of replacing loopback, and `stop` always uses the local address. Restart from the dashboard (`POST /api/system/restart`). Password required for network access. Per-target Network page (`GET /api/targets/{id}/network`). New dependencies rustls and axum-server; sysinfo `network` feature. Dashboard scripts grouped into folders (`js/core`, `js/pages`, `js/target`, `js/settings`); the Settings page now has a side menu with one section per address (`#/settings/<section>`), each a `SET.<name>` function. Release workflow on GitHub Actions. |
| 1.1.0 | 0.6.0 | 2026-11-12 | Windows build is a windowed program: no terminal window, attaches to the parent terminal for command output, opens the dashboard when started twice (3.5). Native About window from the tray menu and the new `about` command; macOS uses the system About panel. Author, homepage and tagline in `Cargo.toml`, `/api/system` (`author`, `homepage`, `tagline`) and the About page. New dependency windows-sys; the manifest now requests common controls 6 and is required at build time. |
| 1.0.0 | 0.5.0 | 2026-11-12 | First version of this file. In this release: application icon embedded in the Windows executable and used as the browser-tab icon; tray icon on Windows and menu-bar icon on macOS with status tooltip and menu (`tray.rs`, `--no-tray`); macOS app bundle script; C runtime linked statically on Windows; per-target Python environment (`pyenv.rs`, new column `portal_targets.python_env`, `GET /api/fs/pyenv`); About page with version, commit and build time (`/api/system` gained `commit`, `built_at`, `sqlite`, `libgit2`, `started_at`). |

Earlier history, before this file existed, is in the Git log: 0.4.0 added metric
history, project detection with generated start scripts and the redesigned New Target
page; before that came the split dashboard, login, start order, rollback, Node support,
automation, multi-process projects, quick commands and backups.
