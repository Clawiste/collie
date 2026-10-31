#!/usr/bin/env bash
# Run the focused pane's project when it has exactly one target; otherwise open the panel to pick one.
# Actions run with cwd = the plugin root, so move to the user's directory first.
set -euo pipefail
cd "$(python3 -c 'import json, os; c = json.loads(os.environ.get("HERDR_PLUGIN_CONTEXT_JSON") or "{}"); print(c.get("focused_pane_cwd") or c.get("workspace_cwd") or os.environ["HOME"])')"
collie="$HERDR_PLUGIN_ROOT/target/release/collie"

name="$("$collie" targets --json | python3 -c 'import json, sys; t = json.load(sys.stdin)["targets"]; print(t[0]["name"] if len(t) == 1 else "")')"
if [ -n "$name" ]; then
  exec "$collie" run "$name"
fi
exec bash "$HERDR_PLUGIN_ROOT/scripts/open.sh"
