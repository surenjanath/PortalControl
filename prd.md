Product Requirement Document (PRD)


Project Name: PortalControl-Core (Native Cross-Platform Automation Server)


1. Executive Summary & Goals

PortalControl-Core is a lightweight, zero-dependency native automation supervisor built in Rust. It functions as a web-based, cross-platform replacement for Windows Task Scheduler, macOS launchd, and manual terminal configurations. It mounts natively over existing file structures to track Git status, execute platform-specific deployment scripts (.bat on Windows, .sh on macOS/Linux), and surface internal logging directly to your status dashboard.

Core Objectives:

• Zero-Dependency Compilation: Compiles to a single lightweight binary file (~15MB) running cleanly with minimal RAM use (<25MB) across Windows and macOS.
• Infrastructure Preservation: Point directly to target project directories without wrapping folders in Docker or altering SQLite source files.
• OS-Aware Task Running: Dynamic execution layers that automatically map workflows to native host shell interfaces (cmd.exe vs /bin/sh).
• Real-Time Log Ingestion: Low-overhead asynchronous streaming of script outputs directly to the browser via WebSockets.

2. System Architecture & Tech Stack

   ┌────────────────────────────────────────────────────────┐
   │                  PortalControl Web UI                  │
   │      (Grid Cards, Status Badges, Live Terminal Box)    │
   └───────────────────────────▲────────────────────────────┘
                               │ WebSockets (Async Streams)
   ┌───────────────────────────▼────────────────────────────┐
   │             Rust Control Plane Core Engine             │
   │         (Tokio Runtime + Axum REST/WS Server)         │
   └───────┬───────────────────┬────────────────────┬───────┘
           │                   │                    │
   ┌───────▼───────┐   ┌───────▼───────┐    ┌───────▼───────┐
   │ Target Portal │   │ Target Portal │    │ Target Portal │
   │  (Windows OS) │   │  (macOS / LX) │   │ (Cross-Plat DB)│
   │  └─ deploy.bat│   │  └─ deploy.sh │    │  └─ db.sqlite │
   └───────────────┘   └───────────────┘    └───────────────┘
• Language & Runtime: Rust (Stable Edition 2021) utilizing the Tokio multi-threaded asynchronous architecture.
• Web Framework: Axum for handling REST APIs and high-performance WebSockets.
• Git Interface Engine: Native C-bindings interface via the git2-rs crate (circumvents needing a system-level Git installer).
• Internal Data Layer: Embedded, thread-safe asynchronous rusqlite database engine to persist manager configurations.

3. Functional Requirements


Module 1: Cross-Platform Target Registration & Matrix

• Target Directory Mounting: Administrators record instances in the registry using the path mechanics native to the host machine:
	• Windows Instance: C:\Users\ICT\Projects\claims_portal ➔ Run: deploy.bat
	• macOS Instance: /Users/ict/projects/claims_portal ➔ Run: deploy.sh
• Health Status Engine: Periodic network probes confirm application status maps (LIVE | SLOW | DOWN) while monitoring thread metrics.

Module 2: Native Git Delta Tracking

• Zero-Shell Git Verification: The engine reads repository objects directly inside .git/ tracking databases.
• State Detection Matrix: The manager matches local HEAD hashes with corresponding remote branches to calculate conditions:
	• UP_TO_DATE: Local and remote SHA-1 signatures match identically.
	• UPDATES_AVAILABLE: Remote tracking branch includes commits ahead of HEAD.
	• DIRTY: Local workspace contains modified or untracked changes.
• Automated Workspace Fetching: Spawns decoupled network workers to query remote repositories every 5 minutes, maintaining precise delta statistics without locking code logic.

Module 3: OS-Aware Asynchronous Process Worker

• Compile-Time Shell Routing: The compiler maps execution layers to target platforms dynamically using internal engine attributes:rust
let (shell_binary, shell_switch) = if cfg!(target_os = "windows") {
    ("cmd.exe", "/C")
} else {
    ("/bin/sh", "-c")
};
Use code with caution.
• Asynchronous Context Locking: Spawns isolated shell command runtimes bound cleanly to the respective directory via .current_dir(working_directory_path).
• WebSocket Console Pipelines: Captured stdout handles split text elements line-by-line using asynchronous streaming buffer readers (tokio::io::BufReader), dispatching items out to connected Web interfaces with sub-millisecond latencies.

Module 4: Integrated Security Profiling Plane

• Multi-Platform Code Analysis: Integrates vulnerability scanners directly into the task runner execution cycle.
• Static & Dependency Analysis: One-click operations run structural inspection checks targeting app code libraries:
	• Executes localized Python analysis configurations (bandit -r <target_path> -f json).
	• Parses package security trees via dependency scanners (pip-audit --format json).
• Log Ingestion Engine: Rust consumes diagnostic payload output logs, formatting results cleanly into your main dashboard grid cards.

4. Relational Storage Schema (Embedded SQLite)

The embedded manager database logs metrics and targets without external server configurations:
sql
-- Application Targets Manifest
CREATE TABLE portal_targets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    working_directory TEXT NOT NULL,
    script_name TEXT NOT NULL,
    git_branch TEXT NOT NULL DEFAULT 'main',
    current_git_status TEXT CHECK(current_git_status IN ('UP_TO_DATE', 'UPDATES_AVAILABLE', 'DIRTY')) DEFAULT 'UP_TO_DATE',
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Process Execution Performance Log
CREATE TABLE process_execution_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER,
    triggered_by TEXT NOT NULL DEFAULT 'SYSTEM_AUTO',
    status TEXT CHECK(status IN ('RUNNING', 'SUCCESS', 'FAILED')) NOT NULL,
    exit_code INTEGER,
    duration_ms INTEGER,
    captured_logs TEXT,
    executed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
);

-- Code Security Metrics
CREATE TABLE security_scan_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER,
    critical_issues_found INTEGER DEFAULT 0,
    vulnerable_packages_found INTEGER DEFAULT 0,
    raw_payload_json TEXT,
    scanned_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(target_id) REFERENCES portal_targets(id) ON DELETE CASCADE
);
Use code with caution.

5. Non-Functional Requirements & Safety

• Total Sandbox Security: Because the manager executes raw shell components, the web engine must drop all public internet network routing profiles. It binds cleanly to local loopbacks (127.0.0.1) or private mesh addresses (100.x.y.z over Tailscale).
• Zombie Process Mitigations: Active threads command strict timeout policies (std::time::Duration). If a deployment pipeline blocks waiting for input parameters, the supervisor terminates the child ID securely at 300 seconds.
• Memory Defenses: Script logging streams directly across WebSockets line-by-line without building massive, unbounded buffers inside your system's active RAM.
