#!/usr/bin/env bash
# uninstall.sh — remove what install.sh put on this machine.
#
#   ./scripts/uninstall.sh            remove the binaries and the launcher
#   ./scripts/uninstall.sh --purge    also remove consent, the skill, reports,
#                                     the published port file and the log
#
# It never closes a browser: the browser is not ours.
set -euo pipefail

PURGE=0
for arg in "$@"; do
  case "$arg" in
    --purge) PURGE=1 ;;
    -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

BIN=${PREFIX:-$HOME/.local}/bin
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}
CONSENT=$CONFIG_DIR/cdp-mcp
SKILL_DIR=$CONFIG_DIR/opencode/skills/cdp-mcp
DATA=$HOME/.local/share/cdp-mcp
RT=${XDG_RUNTIME_DIR:-/tmp}

say() { printf '%s\n' "$*"; }
rmf() { for f in "$@"; do if [ -e "$f" ]; then say "  rm $f"; rm -f "$f"; fi; done; return 0; }
rmd() { for d in "$@"; do if [ -e "$d" ]; then say "  rm -r $d"; rm -rf "$d"; fi; done; return 0; }

say "binaries"
rmf "$BIN/cdp" "$BIN/cdp-mcp" "$BIN/cdp-browser" "$BIN/helium-cdp"

# Harness entries point at the binary; leaving them behind means an agent keeps
# trying to start something that no longer exists.
say "harness entries"
HERE=$(cd -- "$(dirname -- "$0")" && pwd)
if [ -f "$CONFIG_DIR/opencode/opencode.json" ]; then
  python3 "$HERE/drop-harness-entry.py" "$CONFIG_DIR/opencode/opencode.json" cdp || true
fi
command -v claude >/dev/null && claude mcp remove cdp >/dev/null 2>&1 && say "  removed cdp from Claude Code" || true
command -v codex >/dev/null && codex mcp remove cdp >/dev/null 2>&1 && say "  removed cdp from Codex" || true
true

if [ "$PURGE" = 1 ]; then
  say "configuration, skill, data and runtime state"
  rmd "$CONSENT" "$DATA" "$CONFIG_DIR/opencode/skills/cdp-mcp" "$HOME/.claude/skills/cdp-mcp" "$HOME/.codex/skills/cdp-mcp"
  rmf "$RT/helium-cdp.port" "$RT/helium-cdp.log" "$RT/airdb.state"
  say ""
  say "kept: the browser, and this repository."
else
  say ""
  say "kept: $CONSENT"
  say "kept: $SKILL_DIR"
  say "(add --purge to remove those too)"
fi
