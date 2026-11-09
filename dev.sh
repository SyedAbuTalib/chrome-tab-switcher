#!/usr/bin/env zsh
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

echo "Building chrome-tab-switcher (release)..."
cargo build --release

echo "Starting chrome-tab-switcher (auto-reloads whenever target/release/chrome-tab-switcher is rebuilt)..."
exec ./target/release/chrome-tab-switcher "$@"
