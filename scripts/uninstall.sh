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
SKILL_DIR=$CONFIG_DIR/opencode/skills/cdp
DATA=$HOME/.local/share/cdp-mcp
RT=${XDG_RUNTIME_DIR:-/tmp}

say() { printf '%s\n' "$*"; }
rmf() { for f in "$@"; do [ -e "$f" ] && { say "  rm $f"; rm -f "$f"; }; done; }
rmd() { for d in "$@"; do if [ -e "$d" ]; then say "  rm -r $d"; rm -rf "$d"; fi; done; }

say "binaries"
rmf "$BIN/cdp" "$BIN/cdp-mcp" "$BIN/helium-cdp"

if [ "$PURGE" = 1 ]; then
  say "configuration, skill, data and runtime state"
  rmd "$CONSENT" "$SKILL_DIR" "$DATA"
  rmf "$RT/helium-cdp.port" "$RT/helium-cdp.log" "$RT/airdb.state"
  say ""
  say "kept: the browser, and this repository."
else
  say ""
  say "kept: $CONSENT"
  say "kept: $SKILL_DIR"
  say "(add --purge to remove those too)"
fi
