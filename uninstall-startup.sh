#!/usr/bin/env zsh
set -euo pipefail

PLIST_LABEL="com.local.chrome-tab-switcher"
PLIST_PATH="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
APP_DIR="$HOME/Applications/ChromeTabSwitcher.app"

echo "Stopping and removing LaunchAgent ${PLIST_LABEL}..."
launchctl bootout "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
rm -f "$PLIST_PATH"
rm -rf "$APP_DIR"

echo "✅ Uninstalled startup service."
