#!/bin/sh
# Launched by the TV with the launch parameters (JSON) as $1. For the prototype, what to run is
# in `args` next to this script (one argument per line), with optional `env` (shell assignments);
# output goes to a log in the developer temp folder.
cd "$(dirname "$0")"
LOG=/media/developer/temp/dw/app.log
{ echo "=== $(date) launch: $*"; id; echo "APPID=$APPID"; } >> "$LOG" 2>&1
[ -f env ] && . ./env
set --
while IFS= read -r a; do set -- "$@" "$a"; done < args
exec ./displaywire-tv "$@" >> "$LOG" 2>&1
