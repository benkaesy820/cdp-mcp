#!/usr/bin/env bash
# install.sh — build cdp-mcp, install it, and wire up `cdp` and the Helium
# launcher.
#
#   ./install.sh                 install, asking about the optional parts
#   ./install.sh --dry-run       show every step and every question, change nothing
#   ./install.sh --yes           take the defaults without asking (for scripts)
#   ./install.sh --uninstall     remove the binaries, keep consent and skills
#
# Questions are only asked when there is a terminal to ask them in. Piped or
# scripted runs take the defaults and say which ones they took, so nothing hangs.
#
# It never starts a browser, never edits the browser's flags, and writes only to
# $PREFIX/bin, ~/.config/cdp-mcp (consent) and ~/.config/opencode/skills/cdp.
set -euo pipefail

DRY=0
YES=0
UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY=1 ;;
    --yes|-y) YES=1 ;;
    --with-skill) : ;;   # kept for compatibility; the question now asks
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

PREFIX=${PREFIX:-$HOME/.local}
BIN=$PREFIX/bin
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}
CONSENT=$CONFIG_DIR/cdp-mcp/consent.json
SKILL_DIR=$CONFIG_DIR/opencode/skills/cdp
ROOT=$(cd "$(dirname "$0")/.." && pwd)

step() { printf '\n== %s\n' "$*"; }
run() {
  if [ "$DRY" = 1 ]; then printf '   [dry] %s\n' "$*"; else printf '   %s\n' "$*"; "$@"; fi
}
# ask <question> <default y|n> -> sets ANSWER to y or n
ask() {
  local q=$1 def=$2 hint="[y/N]"
  [ "$def" = y ] && hint="[Y/n]"
  if [ "$DRY" = 1 ]; then
    printf '   [dry] %s %s -> %s\n' "$q" "$hint" "$def"
    ANSWER=$def
    return
  fi
  if [ "$YES" = 1 ] || [ ! -t 0 ]; then
    printf '   %s %s -> %s (not asking: %s)\n' "$q" "$hint" "$def" \
      "$([ "$YES" = 1 ] && echo --yes || echo 'no terminal')"
    ANSWER=$def
    return
  fi
  printf '   %s %s ' "$q" "$hint"
  local line=""
  IFS= read -r line || true
  case "${line:-$def}" in
    [Yy]|[Yy][Ee][Ss]) ANSWER=y ;;
    *) ANSWER=n ;;
  esac
}

if [ "$UNINSTALL" = 1 ]; then
  step "remove $BIN/cdp and $BIN/cdp-mcp"
  run rm -f "$BIN/cdp" "$BIN/cdp-mcp"
  step "remove the Helium launcher (only if this installer put it there)"
  [ -f "$BIN/helium-cdp" ] && run rm -f "$BIN/helium-cdp"
  printf '\nkept: %s\nkept: %s\n' "$CONSENT" "$SKILL_DIR"
  printf '(delete those by hand if you mean it)\n'
  exit 0
fi

step "preflight"
for tool in cargo install; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 3; }
  printf '   found %s (%s)\n' "$tool" "$(command -v "$tool")"
done
printf '   prefix: %s\n' "$PREFIX"
[ -d "$BIN" ] || run mkdir -p "$BIN"

step "build (release)"
if [ "$DRY" = 1 ]; then printf '   [dry] cargo build --release (in %s)\n' "$ROOT"
else ( cd "$ROOT" && cargo build --release ); fi

step "install the binary (rename, so a running server is not disturbed)"
run install -m755 "$ROOT/target/release/cdp-mcp" "$BIN/.cdp-mcp.new"
run mv -f "$BIN/.cdp-mcp.new" "$BIN/cdp-mcp"
step "link cdp -> cdp-mcp"
run ln -sf "$BIN/cdp-mcp" "$BIN/cdp"

step "Helium launcher"
if command -v helium-browser >/dev/null; then
  ask "Install the Helium launcher (takes 9222, or a free port, and publishes it)?" y
  if [ "$ANSWER" = y ]; then run install -m755 "$ROOT/scripts/helium-cdp" "$BIN/helium-cdp"; fi
else
  printf '   no helium-browser on PATH; skipping. Chrome/Chromium work too: the port\n'
  printf '   is read from <profile>/DevToolsActivePort.\n'
fi

step "consent"
if [ -f "$CONSENT" ]; then
  printf '   current: %s\n' "$(tr -d '\n' < "$CONSENT" | sed 's/  */ /g' | cut -c1-120)..."
  ask "Keep it? (answer n to walk through the questions again)" y
  if [ "$ANSWER" = n ] && [ -t 0 ] && [ "$DRY" = 0 ]; then "$BIN/cdp-mcp" setup; fi
  [ "$ANSWER" = n ] && [ "$DRY" = 1 ] && printf '   [dry] cdp-mcp setup\n'
else
  ask "Ask the consent questions now (may a browser be started, which one, foreground)?" y
  if [ "$ANSWER" = y ]; then
    if [ "$DRY" = 1 ]; then printf '   [dry] cdp-mcp setup\n'; else "$BIN/cdp-mcp" setup; fi
  else
    printf '   no consent file: cdp-mcp will not start a browser until you run\n'
    printf '   `cdp-mcp setup`.\n'
  fi
fi

step "agent skill"
ask "Install the agent skill into $SKILL_DIR?" n
if [ "$ANSWER" = y ]; then
  run mkdir -p "$SKILL_DIR"
  run install -m644 "$ROOT/skills/cdp/SKILL.md" "$SKILL_DIR/SKILL.md"
else
  printf '   skipped; the file is %s/skills/cdp/SKILL.md\n' "$ROOT"
fi

step "verify"
if [ "$DRY" = 1 ]; then
  printf '   [dry] cdp --version\n   [dry] cdp status\n\nNothing changed.\n'
  exit 0
fi
"$BIN/cdp-mcp" --version
if "$BIN/cdp" status 2>/dev/null; then
  printf '\nok: a browser is publishing DevTools and `cdp` can reach it.\n'
else
  printf '\nok: installed. No browser is publishing DevTools right now:\n'
  printf '   helium-cdp     start one (takes 9222, or a free port if busy)\n'
  printf '   cdp status     then expect one line per tab\n'
fi
printf '\nnext: restart any running agent host so it loads the new server.\n'
