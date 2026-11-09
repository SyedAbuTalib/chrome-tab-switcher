#!/usr/bin/env zsh
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$HOME/Applications/ChromeTabSwitcher.app"
PLIST_LABEL="com.local.chrome-tab-switcher"
PLIST_PATH="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
LOG_DIR="$HOME/Library/Logs/chrome-tab-switcher"

echo "1. Building release binary..."
cd "$REPO_DIR"
cargo build --release

echo "2. Creating macOS .app bundle at $APP_DIR..."
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$LOG_DIR"

cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>chrome-tab-switcher</string>
    <key>CFBundleIdentifier</key>
    <string>${PLIST_LABEL}</string>
    <key>CFBundleName</key>
    <string>ChromeTabSwitcher</string>
    <key>CFBundleDisplayName</key>
    <string>Chrome Tab Switcher</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

cp -f "$REPO_DIR/target/release/chrome-tab-switcher" "$APP_DIR/Contents/MacOS/chrome-tab-switcher"
codesign --force --deep --sign - --identifier "$PLIST_LABEL" "$APP_DIR"

echo "3. Installing LaunchAgent at $PLIST_PATH..."
mkdir -p "$HOME/Library/LaunchAgents"

cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${APP_DIR}/Contents/MacOS/chrome-tab-switcher</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>${LOG_DIR}/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>${LOG_DIR}/stderr.log</string>
</dict>
</plist>
EOF

# Stop any manually running instance first so port 17384 is free
pkill -f "target/release/chrome-tab-switcher" 2>/dev/null || true

# Clear any stale ad-hoc CDHash entry in TCC so macOS prompts cleanly for the updated binary
tccutil reset Accessibility "$PLIST_LABEL" 2>/dev/null || true

# Reload the LaunchAgent in the user's GUI session
if ! launchctl kickstart -k "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null; then
    launchctl bootout "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
    sleep 0.5
    launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH"
fi

echo ""
echo "✅ Installed and started ${PLIST_LABEL}!"
echo "   - App bundle:  $APP_DIR"
echo "   - LaunchAgent: $PLIST_PATH"
echo "   - Logs:        $LOG_DIR/stderr.log"
echo ""
echo "👉 IMPORTANT: Because it now runs independently of Terminal, grant Accessibility"
echo "   permission to '$APP_DIR' in:"
echo "   System Settings -> Privacy & Security -> Accessibility"
