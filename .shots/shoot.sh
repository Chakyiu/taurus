#!/bin/zsh
# Usage: shoot.sh <name> [sleep] — launches the debug build, captures its window, quits it.
# TAURUS_VIEW, TAURUS_IMPORT, TAURUS_EXTRACT, TAURUS_DIALOG, TAURUS_MENU and TAURUS_PLAY pass through;
# the library lives in .shots/lib unless TAURUS_DATA_DIR says otherwise.
cd "$(dirname "$0")"
BIN=$(cd .. && cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/taurus
export TAURUS_DATA_DIR=${TAURUS_DATA_DIR:-$PWD/lib}
"$BIN" >/tmp/taurus-$1.log 2>&1 &
PID=$!
sleep ${2:-3}
ID=$(./winid $PID)
screencapture -x -o -l "$ID" "f-$1.png"
sips -Z 1440 "f-$1.png" >/dev/null
kill $PID
echo "f-$1.png"
