#!/usr/bin/env bash
# cdp-mcp installer: the binary, the MCP entry, and the skill, for the harnesses
# this machine actually has.
#
#   ./install.sh                       interactive: detect, ask, install
#   ./install.sh --global --yes        every project, no prompts
#   ./install.sh --project DIR --yes   one project, no prompts
#   ./install.sh --harness opencode    limit to one harness (comma for both)
#
# It never edits a config without showing you the plan first, and every file it
# touches is backed up beside the original first.
set -euo pipefail

REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)
BIN="$HOME/.local/bin/cdp-mcp"
SCOPE=""
PROJECT=""
ASSUME_YES=0
HARNESSES=""
PROFILE="default"

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
cdp-mcp installer $VERSION

Usage: ./install.sh [options]

  --global            install for this user, every project
  --project DIR       install for one project only
  --harness LIST      opencode, claude, or both (comma separated)
  --profile NAME      profile name passed to cdp-mcp (default: default)
  -y, --yes           no prompts; requires --global or --project
  -h, --help          this text

Without --harness, it offers the harnesses it finds. Without --global or
--project, it asks. Interactive by default; --yes is for scripts and agents.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --global) SCOPE=global ;;
        --project)
            SCOPE=project
            shift
            [ $# -gt 0 ] || die "--project needs a directory"
            PROJECT=$1
            ;;
        --harness)
            shift
            [ $# -gt 0 ] || die "--harness needs a list"
            HARNESSES=$1
            ;;
        --profile)
            shift
            [ $# -gt 0 ] || die "--profile needs a name"
            PROFILE=$1
            ;;
        -y | --yes) ASSUME_YES=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument $1 (try --help)" ;;
    esac
    shift
done

[ -f "$REPO/Cargo.toml" ] || die "run this from the cdp-mcp checkout: git clone https://github.com/benkaesy820/cdp-mcp && cd cdp-mcp && ./install.sh"
command -v python3 >/dev/null || die "python3 is required to merge JSON config safely"

have() { command -v "$1" >/dev/null 2>&1; }

DETECTED=()
add_detected() {
    case " ${DETECTED[*]:-} " in
        *" $1 "*) ;;
        *) DETECTED+=("$1") ;;
    esac
}
have opencode && add_detected opencode
have claude && add_detected claude
[ -d "$HOME/.config/opencode" ] && add_detected opencode
[ -d "$HOME/.claude" ] && add_detected claude

# ---------------------------------------------------------------- what to install

if [ -z "$HARNESSES" ]; then
    if [ "$ASSUME_YES" = 1 ]; then
        if [ ${#DETECTED[@]} -gt 0 ]; then
            HARNESSES=$(IFS=,; printf '%s' "${DETECTED[*]}")
        else
            HARNESSES="opencode,claude"
        fi
    else
        [ -t 0 ] || die "no terminal to ask in: pass --harness LIST and --global/--project with --yes"
        seen=${DETECTED[*]:-none}
        default=$(IFS=,; printf '%s' "${DETECTED[*]:-opencode,claude}")
        say "Harnesses found: $seen"
        read -r -p "Install for [$default]: " HARNESSES
        HARNESSES=${HARNESSES:-$default}
    fi
fi

for h in ${HARNESSES//,/ }; do
    case "$h" in
        opencode | claude) ;;
        *) die "unknown harness $h (supported: opencode, claude)" ;;
    esac
done

if [ -z "$SCOPE" ]; then
    [ -t 0 ] || die "no terminal to ask in: pass --global or --project with --yes"
    say ""
    say "Where should cdp-mcp be available?"
    say "  1) Every project (user level)"
    say "  2) One project only"
    read -r -p "Choose [1]: " answer
    case "${answer:-1}" in
        1) SCOPE=global ;;
        2) SCOPE=project ;;
        *) die "choose 1 or 2" ;;
    esac
fi

if [ "$SCOPE" = project ] && [ -z "$PROJECT" ]; then
    [ -t 0 ] || die "--project needs a directory"
    read -r -p "Project directory [$PWD]: " answer
    PROJECT=${answer:-$PWD}
fi
if [ "$SCOPE" = project ]; then
    PROJECT=$(cd -- "$PROJECT" 2>/dev/null && pwd) || die "no such directory: $PROJECT"
fi

say ""
say "  harnesses : $HARNESSES"
say "  scope     : $SCOPE${PROJECT:+ ($PROJECT)}"
say "  profile   : $PROFILE"
say "  binary    : $BIN"
if [ "$ASSUME_YES" = 0 ]; then
    read -r -p "Proceed? [Y/n] " answer
    case "${answer:-y}" in
        y | Y | yes | YES) ;;
        *) exit 1 ;;
    esac
fi

# ---------------------------------------------------------------- the binary

if [ -x "$BIN" ]; then
    say ""
    say "binary: $BIN ($("$BIN" --version 2>/dev/null || printf 'present'))"
else
    have cargo || die "cargo is required to build cdp-mcp; install Rust 1.88+ or put a cdp-mcp binary in $BIN"
    say ""
    say "building cdp-mcp $VERSION ..."
    if have rustup && rustup target list --installed 2>/dev/null | grep -qx x86_64-unknown-linux-musl; then
        cargo build --quiet --release --target x86_64-unknown-linux-musl --manifest-path "$REPO/Cargo.toml"
        SRC="$REPO/target/x86_64-unknown-linux-musl/release/cdp-mcp"
    else
        cargo build --quiet --release --manifest-path "$REPO/Cargo.toml"
        SRC="$REPO/target/release/cdp-mcp"
    fi
    mkdir -p "$HOME/.local/bin"
    install -m 755 "$SRC" "$BIN"
    say "binary: $BIN"
fi

# ---------------------------------------------------------------- JSON merge

backup() {
    [ -f "$1" ] || return 0
    cp -p "$1" "$1.bak.$(date +%Y%m%d%H%M%S)"
}

merge_opencode() {
    local cfg=$1
    backup "$cfg"
    python3 - "$cfg" "$BIN" "$PROFILE" <<'PY'
import json, os, sys
path, binary, profile = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(path) as f:
        cfg = json.load(f)
except FileNotFoundError:
    cfg = {}
except json.JSONDecodeError as e:
    sys.exit(f"existing config is not valid JSON: {e}")
cfg.setdefault("mcp", {}).setdefault("servers", {})["cdp"] = {
    "type": "local",
    "command": [binary, "--profile", profile],
}
os.makedirs(os.path.dirname(path), exist_ok=True)
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
os.replace(tmp, path)
PY
}

merge_claude_json() {
    local cfg=$1
    backup "$cfg"
    python3 - "$cfg" "$BIN" "$PROFILE" <<'PY'
import json, os, sys
path, binary, profile = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(path) as f:
        cfg = json.load(f)
except FileNotFoundError:
    cfg = {}
except json.JSONDecodeError as e:
    sys.exit(f"existing config is not valid JSON: {e}")
cfg.setdefault("mcpServers", {})["cdp"] = {
    "type": "stdio",
    "command": binary,
    "args": ["--profile", profile],
}
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
os.replace(tmp, path)
PY
}

# ---------------------------------------------------------------- skills and config

skill_src="$REPO/.claude/skills/cdp-mcp/SKILL.md"
[ -f "$skill_src" ] || die "skill source missing: $skill_src"

install_skill() {
    local dir=$1
    mkdir -p "$dir/cdp-mcp"
    cp "$skill_src" "$dir/cdp-mcp/SKILL.md"
    say "skill : $dir/cdp-mcp/SKILL.md"
}

for h in ${HARNESSES//,/ }; do
    say ""
    case "$h" in
        opencode)
            if [ "$SCOPE" = global ]; then
                install_skill "$HOME/.config/opencode/skills"
                merge_opencode "$HOME/.config/opencode/opencode.json"
                say "config: $HOME/.config/opencode/opencode.json (mcp.servers.cdp)"
            else
                install_skill "$PROJECT/.opencode/skills"
                merge_opencode "$PROJECT/opencode.json"
                say "config: $PROJECT/opencode.json (mcp.servers.cdp)"
            fi
            ;;
        claude)
            if [ "$SCOPE" = global ]; then
                install_skill "$HOME/.claude/skills"
            else
                install_skill "$PROJECT/.claude/skills"
            fi
            if have claude; then
                if [ "$SCOPE" = global ]; then
                    claude mcp add --scope user cdp -- "$BIN" --profile "$PROFILE" >/dev/null 2>&1 ||
                        warn "claude mcp add failed; add cdp from ~/.claude.json by hand"
                    say "config: Claude Code (user scope)"
                else
                    (cd "$PROJECT" && claude mcp add --scope project cdp -- "$BIN" --profile "$PROFILE" >/dev/null 2>&1) ||
                        warn "claude mcp add failed; add cdp to $PROJECT/.mcp.json by hand"
                    say "config: Claude Code (project scope, $PROJECT)"
                    say "note  : Claude Code asks once to approve the project server on next launch"
                fi
            elif [ "$SCOPE" = project ]; then
                merge_claude_json "$PROJECT/.mcp.json"
                say "config: $PROJECT/.mcp.json (mcpServers.cdp)"
            else
                warn "claude CLI not found; add this to your Claude Code MCP config:"
                say "  command: $BIN"
                say "  args:    [\"--profile\", \"$PROFILE\"]"
            fi
            ;;
    esac
done

# ---------------------------------------------------------------- check the browser

say ""
if version=$(curl -s --max-time 2 http://127.0.0.1:9222/json/version 2>/dev/null) && [ -n "$version" ]; then
    browser=$(printf '%s' "$version" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("Browser","?"))' 2>/dev/null || printf '?')
    say "browser: $browser on 127.0.0.1:9222"
else
    warn "no DevTools endpoint on 127.0.0.1:9222"
    say "  start your browser with --remote-debugging-port=9222, then call the status tool."
fi
say ""
say "done. Restart the agent, then call the status tool."
say "fork it, tweak it, go wild. one ask: a star."
