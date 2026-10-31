#!/usr/bin/env bash
# Toggle the Collie pane in the current tab: open if absent, focus if unfocused, close if focused.
# herdr-lazygit's open-lazygit.sh logic, without its process and worktree identity checks:
# our pane is recognised by its manifest title alone.
set -euo pipefail
herdr="${HERDR_BIN_PATH:-herdr}"

decision="$(COLLIE_PANES="$("$herdr" pane list 2>/dev/null || true)" \
  COLLIE_CURRENT="$("$herdr" pane current 2>/dev/null || true)" python3 - <<'PY' || echo OPEN
import json, os
try:
    panes = json.loads(os.environ["COLLIE_PANES"])["result"]["panes"]
    cur = json.loads(os.environ["COLLIE_CURRENT"])["result"]["pane"]
except Exception:
    raise SystemExit(print("OPEN"))
mine = [p for p in panes if p.get("label") == "Collie" and p.get("tab_id") == cur.get("tab_id")]
focused = [p for p in mine if p.get("focused")]
print("CLOSE " + focused[0]["pane_id"] if focused else "FOCUS " + mine[0]["pane_id"] if mine else "OPEN")
PY
)"

case "$decision" in
  "FOCUS "*) exec "$herdr" plugin pane focus "${decision#FOCUS }" ;;
  "CLOSE "*) exec "$herdr" plugin pane close "${decision#CLOSE }" ;;
esac

cwd="$(python3 -c 'import json, os; c = json.loads(os.environ.get("HERDR_PLUGIN_CONTEXT_JSON") or "{}"); print(c.get("focused_pane_cwd") or c.get("workspace_cwd") or os.environ["HOME"])')"
exec "$herdr" plugin pane open --plugin collie --entrypoint panel \
  --placement split --direction right --cwd "$cwd" --focus
