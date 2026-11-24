#!/usr/bin/env bash
# install.sh — build cdp-mcp, install it, and wire up `cdp` and the Helium
# launcher. Idempotent: run it again after a `git pull` to upgrade in place.
#
#   ./install.sh --dry-run        show every step, change nothing
#   ./install.sh                  install (asks before touching consent or skills)
#   ./install.sh --with-skill     also install skills/cdp into the agent's skills
#   ./install.sh --uninstall      remove the binaries, keep consent and skills
#
# What it will not do: start a browser, edit the browser's flags, or write
# anything outside $PREFIX/bin, ~/.config/cdp-mcp and (with --with-skill)
# ~/.config/opencode/skills/cdp.
set -euo pipefail

DRY=0
WITH_SKILL=0
UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY=1 ;;
    --with-skill) WITH_SKILL=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
  if [ "$DRY" = 1 ]; then
    printf '   [dry] %s\n' "$*"
  else
    printf '   %s\n' "$*"
    "$@"
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  step "remove $BIN/cdp and $BIN/cdp-mcp"
  run rm -f "$BIN/cdp"
  run rm -f "$BIN/cdp-mcp"
  step "remove the Helium launcher (only if this installer put it there)"
  if [ -f "$BIN/helium-cdp" ]; then run rm -f "$BIN/helium-cdp"; fi
  printf '\nkept: %s (delete by hand if you mean it)\nkept: %s\n' "$CONSENT" "$SKILL_DIR"
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
if [ "$DRY" = 1 ]; then
  printf '   [dry] cargo build --release (in %s)\n' "$ROOT"
else
  ( cd "$ROOT" && cargo build --release )
fi

step "install the binary (rename, so a running server is not disturbed)"
run install -m755 "$ROOT/target/release/cdp-mcp" "$BIN/.cdp-mcp.new"
run mv -f "$BIN/.cdp-mcp.new" "$BIN/cdp-mcp"
step "link cdp -> cdp-mcp"
run ln -sf "$BIN/cdp-mcp" "$BIN/cdp"

step "Helium launcher"
if command -v helium-browser >/dev/null; then
  run install -m755 "$ROOT/scripts/helium-cdp" "$BIN/helium-cdp"
  printf '   it takes 9222 when free, a free port otherwise, and publishes it in\n'
  printf '   $XDG_RUNTIME_DIR/helium-cdp.port, which cdp-mcp reads.\n'
else
  printf '   no helium-browser on PATH; skipping (Chrome/Chromium work too, the\n'
  printf '   port is read from <profile>/DevToolsActivePort)\n'
fi

step "consent"
if [ -f "$CONSENT" ]; then
  printf '   exists: %s (re-run `cdp-mcp setup` to change the answers)\n' "$CONSENT"
elif [ ! -t 0 ]; then
  printf '   no consent file, and stdin is not a terminal. Run `cdp-mcp setup`\n'
  printf '   yourself: it asks whether a browser may be started, which one, and\n'
  printf '   whether a tab may take the foreground.\n'
elif [ "$DRY" = 1 ]; then
  printf '   [dry] cdp-mcp setup  (interactive: launch? which browser? foreground?)\n'
else
  "$BIN/cdp-mcp" setup
fi

step "agent skill"
if [ "$WITH_SKILL" = 1 ]; then
  run mkdir -p "$SKILL_DIR"
  run install -m644 "$ROOT/skills/cdp/SKILL.md" "$SKILL_DIR/SKILL.md"
else
  printf '   skipped. To install it: ./install.sh --with-skill\n'
  printf '   (it is markdown: %s/skills/cdp/SKILL.md)\n' "$ROOT"
fi

step "verify"
if [ "$DRY" = 1 ]; then
  printf '   [dry] cdp --version\n   [dry] cdp status\n'
  printf '\nNothing changed. Run without --dry-run to do it.\n'
  exit 0
fi
"$BIN/cdp-mcp" --version
if "$BIN/cdp" status 2>/dev/null; then
  printf '\nok: a browser is publishing DevTools and `cdp` can reach it.\n'
else
  printf '\nok: installed. No browser is publishing DevTools right now, so start one:\n'
  printf '   helium-cdp              (takes 9222, or a free port if it is busy)\n'
  printf '   cdp status              (should then print one line per tab)\n'
fi
printf '\nnext: restart any running agent host so it loads the new server.\n'
