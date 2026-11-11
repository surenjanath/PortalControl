# PortalControl-Core

A native, single-binary automation supervisor written in Rust. It serves a local web
dashboard that tracks the Git state of your project folders, runs their deployment
scripts through the host's own shell (`cmd.exe /C` on Windows, `/bin/sh -c` on
macOS/Linux), streams the output live over WebSockets, probes application health and
runs security scans. It points at existing folders as they are: no Docker, no wrapping,
no changes to your project's files or databases.

Built from [`prd.md`](prd.md). The dashboard is laid out like a self-hosted deployment
panel: a sidebar, projects, and a page per target with Configuration, Deployments,
Logs, Metrics and Security tabs.

## Dashboard at a glance

| Page | What it shows |
| --- | --- |
| Dashboard | Totals (running, reachable, updates available, failing, security findings), host CPU and memory, target cards grouped by project, recent deployments and activity |
| Projects | Every target with status, Git state, health strip, last deployment and quick actions |
| Target → Configuration | General, Environment Variables, Git Source, Health Check, Scheduled Task, Webhooks, Advanced, Danger Zone |
| Target → Deployments | Every run with trigger, commit, duration and exit code; click one for its full log |
| Project page | The processes of one project with Start / Restart / Stop all and their terminals grouped |
| Target → Commands | One-click maintenance commands (migrate, collectstatic, pip install …) |
| Target → Logs | Live output with filter, stream selector, follow, wrap, copy and download |
| Target → Metrics | CPU and memory of the whole process tree, health response time, run durations |
| Target → Security | bandit and pip-audit findings, with scan history |
| Deployments / Security / Activity | The same information across all targets, plus an event feed |
| Settings | Start at login, manager details, resource charts, the manager's own log, shutdown |

## Projects with several processes

A project is often more than one process: a web server plus a queue cluster or
workers. Each process is its own target (own logs, health, metrics and restart
policy), and targets that share a **project** name belong together:

- The **project page** (click a project name) lists its processes, has *Start all*,
  *Restart all* and *Stop all*, and shows their **terminals together**: one merged,
  colour-labelled stream plus a tab per process.
- For Django projects PortalControl recognises **Django-Q** (`django_q` in settings or
  requirements → `manage.py qcluster`) and **Celery** (a `celery.py` next to
  `settings.py` → worker, plus beat when a beat schedule is configured). They are
  offered as tick boxes on *New target* and under *Configuration → Related Processes*.
- Adding one registers a second service in the same project with restart-on-crash
  on. If no launcher exists yet, a small script (`portalcontrol_qcluster.bat` / `.sh`)
  is created in the project folder; it only runs the detected command, uses the
  project's own virtualenv when there is one, and an existing file is never
  overwritten. This is the one case where PortalControl writes into a project folder.
- Any other kind of process can be added by hand: create a script for it and register
  it as a target with the same project name.

## Quick commands

The **Commands** tab of a target offers one-click maintenance commands for the project
type it finds in the folder:

| Project | Commands |
| --- | --- |
| Django (`manage.py`) | System check, Show migrations, Check for model changes, Apply migrations, Collect static files, Clear expired sessions |
| Python (`requirements.txt`) | Install requirements, List outdated packages |

A command runs in the working directory with the target's environment variables, using
the project's virtualenv when it has one. It streams to the log and is recorded under
Deployments like any other run, and it runs **next to a live service** without stopping
it. Commands that change something ask first, and are stopped after 15 minutes. Only
this fixed list can be run: the dashboard never executes text you type.

## Database backups

Under *Configuration → Database Backups*, name the project's SQLite file (for example
`db.sqlite3`) to get:

- **Back up now**: a consistent snapshot taken through a read-only connection
  (`VACUUM INTO`), safe while the application is running.
- **Back up before every run**: a snapshot before the script starts and before the
  *Apply migrations* command.
- **Restore**: replaces the database with a snapshot. The target must be stopped, and
  the current state is snapshotted first so a restore can be undone.

Snapshots are kept in PortalControl's data directory (`backups/<target id>/`), outside
the project; the newest 14 per target are kept. They are not removed when a target is
deleted.

## Everyday conveniences

- **Command palette.** `Ctrl+K` (or `/`) jumps to any page, project or target and can
  start, stop or restart a target.
- **Desktop notifications.** Opt in under Settings to be told when a run fails, a
  service goes down or crashes, or a run finishes while the tab is in the background.
- **Export / import.** Settings can save every target's configuration to a JSON file
  and load it on another machine. Already-registered targets are skipped; ones whose
  folder does not exist there are reported.

## Automation features

- **Environment variables.** A `.env`-style block per target, set for the script and
  everything it starts. Stored in PortalControl's database, not in the project folder.
- **Scheduled task.** A five-field cron expression in local time (`0 2 * * *`), with a
  preview of the next runs. A scheduled task that is still running is skipped; a
  scheduled service is restarted.
- **Pull and deploy.** *Pull* fast-forwards the working tree to `origin/<branch>`. It
  only ever fast-forwards: it refuses if tracked files were edited locally, if the
  branch has commits that are not on origin, or if another branch is checked out.
  Untracked files (databases, media) are left alone.
- **Auto-deploy.** When the 5-minute fetch finds new commits, pull and run the script
  (a running service is restarted). A failure is reported once per commit.
- **Deploy webhook.** A per-target URL with a random token; `POST` (or `GET`) to it to
  pull and deploy. Add `?pull=0` to skip the pull. It is reachable only from this
  machine or the private network, like the rest of the server.
- **Restart on crash.** A service that exits on its own is started again after 5
  seconds; after 5 crashes in 10 minutes automatic restart is suspended.
- **Start with PortalControl.** Launch the target whenever the manager starts.
- **Activity feed.** Deployments, health changes, new commits, scans and automation
  events (the last 300, kept in memory).

## Quick start

Requires [Rust](https://rustup.rs) to build. Nothing else is needed at runtime: SQLite
and libgit2 are compiled into the binary, so no Git installation is required.

**Windows**

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1
```

**macOS / Linux**

```sh
sh scripts/install-autostart.sh
```

Either script builds the release binary, installs it to a stable per-user location,
registers it to start at login and starts it. Then open <http://127.0.0.1:7878/>.

To run it by hand instead:

```sh
cargo build --release
./target/release/portalcontrol --open        # foreground, opens the dashboard
```

## Starting automatically at login

| Platform | Mechanism | Installed by |
| --- | --- | --- |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\PortalControl` (per user, no admin) | `portalcontrol install-autostart` |
| macOS | launchd agent `~/Library/LaunchAgents/com.portalcontrol.core.plist` | `portalcontrol install-autostart` |
| Linux | systemd user unit `~/.config/systemd/user/portalcontrol.service` | `portalcontrol install-autostart` |

```sh
portalcontrol install-autostart [--port 9000] [--bind 100.x.y.z]   # options are remembered
portalcontrol autostart-status
portalcontrol uninstall-autostart
```

The entry points at the executable you ran the command from, so install the binary
somewhere permanent first (the scripts in `scripts/` do this for you). On Windows the
login launch uses `serve --detach`, which runs without a console window.

Targets can also be started automatically: tick **Start this target automatically when
PortalControl starts** on a target and it is launched (as `SYSTEM_AUTO`) whenever the
manager comes up. Together with login autostart this replaces a Task Scheduler /
launchd entry per project.

## Command line

```
portalcontrol [COMMAND] [OPTIONS]

serve                 Run the manager and web dashboard (default)
stop                  Stop a running manager (same --bind/--port)
install-autostart     Start PortalControl automatically at login
uninstall-autostart   Remove the login autostart entry
autostart-status      Show whether autostart is installed

--bind <IP>           Address to listen on (default 127.0.0.1)
--port <PORT>         Port to listen on (default 7878)
--data-dir <DIR>      Where the SQLite database and log file live
--detach              Run in the background without a console window
--open                Open the dashboard in the default browser
```

Data lives in `%LOCALAPPDATA%\PortalControl` (Windows),
`~/Library/Application Support/PortalControl` (macOS) or
`~/.local/share/portalcontrol` (Linux): `portalcontrol.db` and `portalcontrol.log`.

## Registering a target

Click **New target**. Type the working directory first: PortalControl inspects it and
shows the scripts it found, the project type and the Git branch, and fills in what it
can. The remaining settings live on the target's Configuration tab.

| Field | Meaning |
| --- | --- |
| Working directory | Absolute path of the project on this machine, e.g. `C:\Users\ICT\Projects\claims_portal` or `/Users/ict/projects/claims_portal` |
| Project | A label used to group targets on the dashboard |
| Script | A script inside that directory, e.g. `deploy.bat` / `deploy.sh` |
| Git branch | Branch compared against `origin/<branch>` (default `main`) |
| Health URL | Optional URL probed every 15 s |
| Run mode | **Task**: runs to completion and is killed at the timeout (default 300 s). **Service**: long-running process (e.g. a dev server) that stays up until stopped |
| Autostart | Launch this target whenever PortalControl starts |

## What each module does

**Health status engine.** Every 15 seconds each health URL is probed:
`LIVE` (answered in under 1 s), `SLOW` (answered, 1 s or more), `DOWN` (no connection,
timeout after 5 s, or HTTP 5xx). For `https://` URLs only TCP reachability is checked,
because no TLS stack is linked in. The header shows manager metrics (worker threads,
running processes, uptime).

**Native Git delta tracking.** Repositories are read directly with libgit2; the `git`
executable is never spawned. The working tree is inspected every 30 seconds and
`origin` is fetched every 5 minutes (and on **Fetch**):

| State | Condition |
| --- | --- |
| `DIRTY` | Modified or untracked files in the workspace (takes priority) |
| `UPDATES_AVAILABLE` | `origin/<branch>` has commits that `HEAD` does not |
| `UP_TO_DATE` | Neither of the above |

Fetching only updates remote-tracking refs and never touches your files. The working
tree changes only when you press **Pull** / **Pull & Deploy**, enable auto-deploy or
call the deploy webhook, and then only as a clean fast-forward. Private remotes use
your configured Git credential helper.

**Metrics.** Every 5 seconds the manager samples host CPU and memory, its own usage
and, for each running target, the CPU and memory of the whole process tree (15 minutes
are kept in memory).

**Process worker.** Scripts run with the working directory set to the target folder,
stdin closed (a script waiting for input cannot block forever), and
`PYTHONUNBUFFERED=1` so Python output arrives line by line. stdout and stderr are
streamed to the dashboard as they are produced. **Stop**, the timeout, deleting a
target and shutting down the manager all terminate the whole process tree.

**Security scans.** **Scan** runs both tools and stores the report:

- `bandit -r <target_path> -f json` (virtualenvs, `node_modules` and `.git` excluded);
  HIGH-severity findings are counted as critical issues.
- `pip-audit --format json` against `requirements.txt` if present, else the project's
  virtualenv, else the ambient Python environment.

Both must be installed for the Python on your `PATH`: `pip install bandit pip-audit`.
If one is missing the report says so instead of failing silently.

## Safety model

PortalControl executes shell scripts, so it is deliberately not reachable from the
internet:

- It refuses to bind to anything but loopback, private-LAN or Tailscale
  (`100.64.0.0/10`) addresses. `--bind 0.0.0.0` is rejected.
- Requests whose `Host` header is not `localhost` or a private IP are rejected, which
  blocks DNS-rebinding attacks. Requests carrying a foreign `Origin` are rejected, so a
  web page you visit cannot drive the API.
- Only a script file that exists inside the registered directory can be run; script
  names with path traversal or shell metacharacters are rejected.
- Tasks are killed at their timeout (300 s by default).
- Output is streamed line by line; lines are capped at 8 KB and only the last 2,000
  lines (512 KB) of a run are kept for the history.

There is no login: anyone who can reach the bound address can run the registered
scripts. Keep it on `127.0.0.1` unless the network (e.g. your tailnet) is trusted.

## HTTP API

| Method | Path | Purpose |
| --- | --- | --- |
| GET / POST | `/api/targets` | List / register targets |
| GET / PUT / DELETE | `/api/targets/{id}` | Read / update / remove a target |
| POST | `/api/targets/{id}/run`, `/stop` | Start / stop the script |
| GET | `/api/targets/{id}/logs` | Live tail, or the last run's captured output |
| POST | `/api/targets/{id}/restart` | Stop (if running) and start again |
| POST | `/api/targets/{id}/deploy` | Fast-forward pull, then run (`?pull=0` to skip the pull) |
| GET / DELETE | `/api/targets/{id}/history` | Last 100 executions / clear finished runs |
| GET / POST | `/api/targets/{id}/scan` | Latest security report / start a scan |
| GET | `/api/targets/{id}/scans` | Scan summaries, newest first |
| POST | `/api/targets/{id}/git/refresh` | Fetch `origin` and re-inspect now |
| POST | `/api/targets/{id}/git/pull` | Fast-forward to `origin/<branch>` |
| GET | `/api/targets/{id}/git/log`, `/git/changes` | Recent commits / changed files |
| POST | `/api/targets/{id}/health/check` | Probe now |
| GET | `/api/targets/{id}/health/history` | Last 240 probes |
| GET | `/api/targets/{id}/metrics` | CPU / memory samples of the process tree |
| POST / DELETE | `/api/targets/{id}/webhook` | Generate / disable the deploy webhook token |
| GET / POST | `/api/webhooks/deploy/{token}` | Deploy via webhook |
| GET | `/api/targets/{id}/commands` | Quick commands available for the target |
| POST | `/api/targets/{id}/commands/{command}`, `/commands/stop` | Run a quick command / stop the running one |
| GET / POST | `/api/targets/{id}/backups` | List database snapshots / take one now |
| POST / DELETE | `/api/targets/{id}/backups/{file}/restore`, `/backups/{file}` | Restore / delete a snapshot |
| POST | `/api/fs/launcher` | Create the launcher script of a detected worker |
| GET / POST | `/api/export`, `/api/import` | Export / import target configuration |
| GET | `/api/runs`, `/api/runs/{id}` | Recent executions of all targets / one with its logs |
| GET | `/api/activity` | Activity feed |
| GET | `/api/cron?expr=` | Validate a schedule and preview its next runs |
| GET | `/api/fs/inspect?dir=` | Scripts, project type and Git state of a directory |
| GET | `/api/system`, `/api/system/metrics`, `/api/system/log` | Manager details, host samples, manager log tail |
| POST / DELETE | `/api/system/autostart` | Enable / disable start at login |
| POST | `/api/system/shutdown` | Stop the manager and its child processes |
| GET | `/ws` | WebSocket event stream (`log`, `run_started`, `run_finished`, `health`, `git`, `scan_*`, `activity`) |

## Development

```sh
cargo test          # unit + integration tests (real scripts, real git repos, real sockets)
cargo run -- --port 7979 --data-dir ./dev-data
```

The source is in `src/` (`runner.rs`, `git.rs`, `health.rs`, `security.rs`,
`automation.rs`, `metrics.rs`, `backup.rs`, `api.rs`, `db.rs`, `autostart.rs`, `net.rs`) and the dashboard is the single file
`web/index.html`, embedded into the binary at compile time.
