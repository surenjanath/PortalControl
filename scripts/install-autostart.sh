#!/bin/sh
# Build, install and register PortalControl to start automatically at login.
#
#   macOS: launchd agent  ~/Library/LaunchAgents/com.portalcontrol.core.plist
#   Linux: systemd user unit  ~/.config/systemd/user/portalcontrol.service
#
# Usage:
#   scripts/install-autostart.sh                 # build, install, enable, start
#   scripts/install-autostart.sh --port 9000     # extra options are passed to `serve`
#   scripts/install-autostart.sh --uninstall
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${PORTALCONTROL_BIN_DIR:-$HOME/.local/bin}"
EXE="$BIN_DIR/portalcontrol"

if [ "${1:-}" = "--uninstall" ]; then
    if [ -x "$EXE" ]; then
        "$EXE" uninstall-autostart
    else
        echo "portalcontrol is not installed in $BIN_DIR; nothing to do."
    fi
    exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo was not found. Install Rust from https://rustup.rs and run this script again." >&2
    exit 1
fi

echo "Building release binary..."
(cd "$ROOT" && cargo build --release)

mkdir -p "$BIN_DIR"
# Stop a running copy before replacing the binary (ignore if none is running).
if [ -x "$EXE" ]; then
    "$EXE" stop "$@" >/dev/null 2>&1 || true
fi
cp "${CARGO_TARGET_DIR:-$ROOT/target}/release/portalcontrol" "$EXE"
chmod +x "$EXE"
echo "Installed $EXE"

# On macOS and Linux the service manager starts the process itself.
"$EXE" install-autostart "$@"
