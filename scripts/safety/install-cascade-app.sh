#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/install-cascade-app.sh
# install-cascade-app.sh — install .desktop launcher for the control app.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPTS="$REPO/scripts/safety"
TEMPLATE="$SCRIPTS/ramshared-cushion.desktop.in"

chmod +x "$SCRIPTS/cascade-app.sh" \
  "$SCRIPTS/cascade-preflight.sh" \
  "$SCRIPTS/cascade-up.sh" \
  "$SCRIPTS/cascade-down.sh" \
  "$SCRIPTS/install-cascade-boot.sh" \
  "$SCRIPTS/uninstall-cascade-boot.sh" 2>/dev/null || true

if [[ "$(id -u)" -eq 0 ]]; then
  DEST_DIR="/usr/local/share/applications"
else
  DEST_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
fi
mkdir -p "$DEST_DIR"
OUT="$DEST_DIR/ramshared-cushion.desktop"

sed -e "s|@SCRIPTS_PATH@|$SCRIPTS|g" "$TEMPLATE" > "$OUT"
chmod 0644 "$OUT"

# Refresh menu cache if tools exist
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$DEST_DIR" 2>/dev/null || true
fi

echo "Installed launcher: $OUT"
echo "Open it from the app menu as “RamShared Cushion”,"
echo "or run:  $SCRIPTS/cascade-app.sh --gui"
echo
echo "CLI:  $SCRIPTS/cascade-app.sh status|check|start|stop"
