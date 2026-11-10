# PortalControl-Core

A native, single-binary automation supervisor written in Rust. It serves a local web
dashboard that tracks the Git state of your project folders, runs their deployment
scripts through the host's own shell (`cmd.exe /C` on Windows, `/bin/sh -c` on
macOS/Linux), streams the output live over WebSockets, probes application health and
runs security scans. It points at existing folders as they are: no Docker, no wrapping,
no changes to your project's files or databases.

Built from [`prd.md`](prd.md).

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

Click **+ Add target** and give:

| Field | Meaning |
| --- | --- |
| Working directory | Absolute path of the project on this machine, e.g. `C:\Users\ICT\Projects\claims_portal` or `/Users/ict/projects/claims_portal` |
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

Fetching only updates remote-tracking refs; the working tree is never modified.
Private remotes use your configured Git credential helper.

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
| GET | `/api/targets/{id}/history` | Last 50 executions |
| GET / POST | `/api/targets/{id}/scan` | Latest security report / start a scan |
| POST | `/api/targets/{id}/git/refresh` | Fetch `origin` and re-inspect now |
| POST | `/api/targets/{id}/health/check` | Probe now |
| GET | `/api/runs/{id}` | One execution with its captured logs |
| GET | `/api/system` | Manager metrics |
| POST | `/api/system/shutdown` | Stop the manager and its child processes |
| GET | `/ws` | WebSocket event stream (`log`, `run_started`, `run_finished`, `health`, `git`, `scan_*`) |

## Development

```sh
cargo test          # unit + integration tests (real scripts, real git repos, real sockets)
cargo run -- --port 7979 --data-dir ./dev-data
```

The source is in `src/` (`runner.rs`, `git.rs`, `health.rs`, `security.rs`, `api.rs`,
`db.rs`, `autostart.rs`, `net.rs`) and the dashboard is the single file
`web/index.html`, embedded into the binary at compile time.
