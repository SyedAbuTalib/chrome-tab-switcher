#!/usr/bin/env zsh
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$HOME/Applications/ChromeTabSwitcher.app"
PLIST_LABEL="com.local.chrome-tab-switcher"
PLIST_PATH="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
LOG_DIR="$HOME/Library/Logs/chrome-tab-switcher"
CERT_NAME="ChromeTabSwitcher Local Signer"
LOGIN_KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

ensure_local_signing_cert() {
    if security find-certificate -c "$CERT_NAME" "$LOGIN_KEYCHAIN" >/dev/null 2>&1; then
        return 0
    fi

    echo "   Generating persistent local code-signing certificate ($CERT_NAME)..."
    local tmp_dir
    tmp_dir="$(mktemp -d)"
    cat > "$tmp_dir/openssl.cnf" <<EOF
[ req ]
distinguished_name = req_dn
x509_extensions = codesign_ext
prompt = no

[ req_dn ]
CN = $CERT_NAME

[ codesign_ext ]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:FALSE
EOF

    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$tmp_dir/key.pem" \
        -out "$tmp_dir/cert.pem" \
        -days 3650 \
        -config "$tmp_dir/openssl.cnf" 2>/dev/null

    openssl pkcs12 -export \
        -out "$tmp_dir/cert.p12" \
        -inkey "$tmp_dir/key.pem" \
        -in "$tmp_dir/cert.pem" \
        -passout pass:localdev \
        -legacy 2>/dev/null || \
    openssl pkcs12 -export \
        -out "$tmp_dir/cert.p12" \
        -inkey "$tmp_dir/key.pem" \
        -in "$tmp_dir/cert.pem" \
        -passout pass:localdev

    security import "$tmp_dir/cert.p12" \
        -k "$LOGIN_KEYCHAIN" \
        -P "localdev" \
        -T /usr/bin/codesign >/dev/null
    rm -rf "$tmp_dir"
}

echo "1. Building release binary..."
cd "$REPO_DIR"
cargo build --release

# Check if the existing bundle already uses our persistent certificate before overwriting it.
# NOTE: deliberately not `codesign ... | grep -q`: grep -q exits on first match, codesign then
# dies with SIGPIPE (141), and `pipefail` turns that into a false negative -> TCC reset every run.
HAD_PERSISTENT_CERT=0
if [[ -d "$APP_DIR" ]]; then
    existing_signature="$(codesign -dvvv "$APP_DIR" 2>&1 || true)"
    if [[ "$existing_signature" == *"Authority=${CERT_NAME}"* ]]; then
        HAD_PERSISTENT_CERT=1
    fi
fi

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

ensure_local_signing_cert
codesign --force --deep --sign "$CERT_NAME" --identifier "$PLIST_LABEL" "$APP_DIR"

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

# Only reset TCC if migrating from an older ad-hoc signature (or if --reset-tcc is passed)
if [[ "$HAD_PERSISTENT_CERT" -eq 0 ]] || [[ "${1:-}" == "--reset-tcc" ]]; then
    echo "   Resetting Accessibility entry once to bind persistent certificate..."
    tccutil reset Accessibility "$PLIST_LABEL" 2>/dev/null || true
fi

# Reload the LaunchAgent in the user's GUI session (bootout + bootstrap refreshes launchd's
# cached Lightweight Code Requirement after replacing and re-signing the binary).
launchctl bootout "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
sleep 0.5
launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH"

echo ""
echo "✅ Installed and started ${PLIST_LABEL} (signed with '${CERT_NAME}')!"
echo "   - App bundle:  $APP_DIR"
echo "   - LaunchAgent: $PLIST_PATH"
echo "   - Logs:        $LOG_DIR/stderr.log"
if [[ "$HAD_PERSISTENT_CERT" -eq 0 ]]; then
    echo ""
    echo "👉 ONE-TIME STEP: Grant Accessibility permission to '$APP_DIR' in:"
    echo "   System Settings -> Privacy & Security -> Accessibility"
    echo "   (Future rebuilds will preserve this permission automatically!)"
fi
