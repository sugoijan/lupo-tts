#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# End-to-end smoke run of the OBS plugin on macOS:
#   build + install the plugin, temporarily enable obs-websocket (with a random
#   password) in the user's OBS config, launch OBS, run obs-smoke.mjs, quit OBS,
#   restore the websocket config and the user's plugin database, and print the
#   plugin's lines from the OBS log. If the script is killed hard, the database
#   backup is left in target/obs-smoke/lupo-db.backup.
# Requires: OBS 31 in /Applications, node >= 22, ffmpeg (for the recording check).
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT_DIR=${OUT_DIR:-"$ROOT/target/obs-smoke"}
CFG="$HOME/Library/Application Support/obs-studio/plugin_config/obs-websocket/config.json"
mkdir -p "$OUT_DIR"

if pgrep -x OBS >/dev/null; then
  echo "OBS is running; quit it first." >&2
  exit 1
fi

RUN_STARTED=$(date +%s)
(cd "$ROOT" && cargo xtask bundle-obs --install ${RELEASE:+--release})

# The smoke run must never touch the user's plugin state (Twitch login, API
# keys, queue): move the plugin database aside and restore it afterwards. The
# trap is installed BEFORE anything is moved or rewritten, and the flags keep
# it from deleting state it never touched, so a failure at any point (missing
# websocket config, openssl, …) cannot strand the user's database or leave a
# half-written config behind.
DB_DIR="$HOME/Library/Application Support/obs-studio/plugin_config/lupo"
DB_BACKUP="$OUT_DIR/lupo-db.backup"
CFG_BACKUP="$OUT_DIR/obs-websocket-config.backup.json"
DB_MOVED=0
CFG_SAVED=0
restore() {
  if [ "$CFG_SAVED" = 1 ]; then
    if [ -f "$CFG_BACKUP" ]; then
      cp "$CFG_BACKUP" "$CFG"
    else
      # No config existed before the run; remove the one we generated.
      rm -f "$CFG"
    fi
  fi
  if [ "$DB_MOVED" = 1 ]; then
    rm -rf "$DB_DIR"
    if [ -d "$DB_BACKUP" ]; then
      mv "$DB_BACKUP" "$DB_DIR"
    fi
  fi
}
trap restore EXIT

rm -rf "$DB_BACKUP"
if [ -d "$DB_DIR" ]; then
  mv "$DB_DIR" "$DB_BACKUP"
fi
DB_MOVED=1

rm -f "$CFG_BACKUP"
if [ -f "$CFG" ]; then
  cp "$CFG" "$CFG_BACKUP"
fi
CFG_SAVED=1
PASSWORD=$(openssl rand -hex 12)
mkdir -p "$(dirname "$CFG")"
python3 - "$CFG" "$PASSWORD" <<'PY'
import json, os, sys
path, password = sys.argv[1], sys.argv[2]
cfg = {}
if os.path.exists(path):
    try:
        cfg = json.load(open(path))
    except ValueError:
        cfg = {}
cfg.update(server_enabled=True, auth_required=True, server_password=password)
json.dump(cfg, open(path, "w"), indent=2)
PY

# --disable-shutdown-check: skip the safe-mode prompt an earlier crash would
# otherwise show, which blocks plugin loading and hangs the run.
open -a OBS --args --disable-shutdown-check
i=0
until nc -z 127.0.0.1 4455 2>/dev/null; do
  i=$((i + 1)); [ "$i" -lt 60 ] || { echo "obs-websocket did not come up (is OBS showing a dialog?)" >&2; osascript -e 'tell application "OBS" to quit' >/dev/null 2>&1 || true; exit 1; }
  sleep 1
done
sleep 3

status=0
OBS_WS_PASSWORD="$PASSWORD" OUT_DIR="$OUT_DIR" node "$ROOT/obs/smoke/obs-smoke.mjs" || status=$?

# Visual evidence of the dock: a screenshot of the OBS window (needs Screen
# Recording permission for the terminal; silently skipped otherwise).
screencapture -x "$OUT_DIR/obs-window.png" 2>/dev/null || true

sleep 1
osascript -e 'tell application "OBS" to quit' >/dev/null 2>&1 || true
i=0
while pgrep -x OBS >/dev/null; do
  i=$((i + 1)); [ "$i" -lt 30 ] || { echo "OBS did not quit" >&2; break; }
  sleep 1
done

# OBS must have shut down cleanly: its log ends with the leak count and no
# crash report appeared during the run.
LOG=$(ls -t "$HOME/Library/Application Support/obs-studio/logs/"*.txt | head -1)
if ! grep -q "Number of memory leaks" "$LOG"; then
  echo "FAIL OBS did not shut down cleanly (no leak count in the log)"; status=1
fi
for report in "$HOME/Library/Logs/DiagnosticReports/"OBS*.ips; do
  [ -f "$report" ] || continue
  if [ "$(stat -f %m "$report")" -ge "$RUN_STARTED" ]; then
    echo "FAIL OBS crash report during the run: $report"; status=1
  fi
done
echo "---- [lupo] lines from $LOG"
grep -n "\[lupo\]\|audio buffering\|Number of memory leaks" "$LOG" || true
echo "---- native dock"
grep -n "native dock" "$LOG" || echo "(none)"
echo "---- panic containment lines"
grep -n "panic" "$LOG" || echo "(none)"
exit $status
