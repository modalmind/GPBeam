#!/usr/bin/env bash
#
# Build GPBeam and install it to /Applications as the ONE registered copy.
#
# `tauri build` leaves a second GPBeam.app under target/release/bundle/macos/.
# macOS LaunchServices auto-registers that build artifact, so Launchpad and
# Spotlight start showing two GPBeams. This script builds, installs to
# /Applications, then removes the build artifact and unregisters every stray
# copy — leaving a single, valid, ad-hoc-signed app.
#
# Usage:  ./scripts/macos-install.sh
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="GPBeam.app"
BUILT="$REPO_ROOT/target/release/bundle/macos/$APP_NAME"
DEST="/Applications/$APP_NAME"
LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

# rustup toolchains live here; make cargo/tauri visible in non-login shells.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

# Resolve a Tauri CLI: standalone `tauri`, the `cargo tauri` subcommand, or a
# project-local install — in that order.
if command -v tauri >/dev/null 2>&1; then
  TAURI=(tauri)
elif cargo tauri --version >/dev/null 2>&1; then
  TAURI=(cargo tauri)
elif [ -x ui/node_modules/.bin/tauri ]; then
  TAURI=(ui/node_modules/.bin/tauri)
else
  echo "error: Tauri CLI not found. Install it with: npm i -g '@tauri-apps/cli@^2'" >&2
  exit 1
fi

echo "==> Building $APP_NAME (release, app bundle only)"
# --bundles app skips the slow .dmg step; we install straight from the .app.
"${TAURI[@]}" build --bundles app

echo "==> Installing to $DEST"
# Quit a running copy so the bundle is replaced cleanly (ignored if not running).
osascript -e 'tell application "GPBeam" to quit' 2>/dev/null || true
rm -rf "$DEST"
ditto "$BUILT" "$DEST"

echo "==> Re-signing (ad-hoc) so the code seal is valid after the copy"
codesign --force --deep --sign - "$DEST"
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

echo "==> Removing the build artifact so it can't register as a duplicate"
"$LSREG" -u "$BUILT" 2>/dev/null || true
rm -rf "$BUILT"

echo "==> Re-registering the installed copy and clearing stale registrations"
"$LSREG" -f "$DEST" 2>/dev/null || true
shopt -s nullglob
for stray in /Volumes/dmg.*/"$APP_NAME"; do
  "$LSREG" -u "$stray" 2>/dev/null || true
done
shopt -u nullglob

echo "==> Done. Registered copies of $APP_NAME (want exactly one — /Applications):"
"$LSREG" -dump 2>/dev/null | grep -i "$APP_NAME" | grep -i 'path:' | sort -u || true
