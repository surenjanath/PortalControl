#!/bin/sh
# Build PortalControl.app: a double-clickable macOS application with the
# PortalControl icon. It starts the manager and lives in the menu bar (no
# Dock icon, no terminal window).
#
# Run on a Mac, from anywhere inside the repository:
#     sh scripts/make-macos-app.sh [output-folder]
# The app is written to the output folder (default: the build folder).
set -eu

cd "$(dirname "$0")/.."

# PORTALCONTROL_BINARY names a program that is already built (the release
# workflow passes its universal build); otherwise build one here.
BUILD="${CARGO_TARGET_DIR:-target}/release"
BINARY="${PORTALCONTROL_BINARY:-}"
if [ -z "$BINARY" ]; then
    cargo build --release
    BINARY="$BUILD/portalcontrol"
fi
APP="${1:-$BUILD}/PortalControl.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/portalcontrol"
chmod +x "$APP/Contents/MacOS/portalcontrol"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PortalControl</string>
  <key>CFBundleDisplayName</key><string>PortalControl</string>
  <key>CFBundleIdentifier</key><string>io.portalcontrol.app</string>
  <key>CFBundleExecutable</key><string>portalcontrol</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# An ad-hoc signature lets the bundle start on Apple Silicon.
codesign --force --deep --sign - "$APP" 2>/dev/null || echo "note: codesign was not run; the app may need it on Apple Silicon"

echo "Built $APP"
echo "Open it with:  open \"$APP\""
