#!/usr/bin/env bash
set -euo pipefail

# session-analyzer installer — installs the `ssa` binary.
#
#   ./install.sh                 build from source (needs Rust) and install
#   ./install.sh --no-build      install a prebuilt binary sitting next to this script
#   ./install.sh --bin-dir DIR   install into DIR (default: ~/.local/bin)
#
# Env: SSA_BIN_DIR overrides the install dir (same as --bin-dir).

BIN="ssa"
BIN_DIR="${SSA_BIN_DIR:-$HOME/.local/bin}"
NO_BUILD=false
# ask | yes | no — whether to install SKILL.md as a global Claude Code skill.
SKILL="${SSA_INSTALL_SKILL:-ask}"
# Claude Code honours CLAUDE_CONFIG_DIR; follow it so a relocated config still works.
CLAUDE_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; BOLD='\033[1m'; NC='\033[0m'
else
  RED='' GREEN='' YELLOW='' BOLD='' NC=''
fi
info()  { echo -e "${GREEN}==>${NC} ${BOLD}$*${NC}"; }
warn()  { echo -e "${YELLOW}==> WARNING:${NC} $*"; }
error() { echo -e "${RED}==> ERROR:${NC} $*" >&2; }

usage() {
  cat <<EOF
Usage: install.sh [OPTIONS]

Install the \`ssa\` (session-analyzer) binary.

Options:
  --no-build        Install a prebuilt binary next to this script (used by release tarballs)
  --bin-dir DIR     Install directory (default: \$SSA_BIN_DIR or ~/.local/bin)
  --skill           Also install SKILL.md as a global Claude Code skill, without asking
  --no-skill        Never install the skill, and do not ask
  -h, --help        Show this help

The skill teaches Claude Code how to drive \`ssa\`. With neither flag, you are asked —
but only when a terminal is attached, so piped and CI installs never block. Set
\$SSA_INSTALL_SKILL=yes|no for the same effect as the flags.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) NO_BUILD=true; shift ;;
    --bin-dir)  BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
    --skill)    SKILL=yes; shift ;;
    --no-skill) SKILL=no; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) error "unknown option: $1"; usage; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Locate the binary to install.
if [[ "$NO_BUILD" == true ]]; then
  if   [[ -f "$SCRIPT_DIR/$BIN" ]]; then SRC="$SCRIPT_DIR/$BIN"
  elif [[ -f "$SCRIPT_DIR/target/release/$BIN" ]]; then SRC="$SCRIPT_DIR/target/release/$BIN"
  else error "no prebuilt '$BIN' found next to install.sh — omit --no-build to build from source"; exit 1
  fi
else
  command -v cargo >/dev/null 2>&1 || {
    error "cargo (Rust) not found. Install it from https://rustup.rs, or use --no-build with a release tarball."
    exit 1
  }
  info "Building release binary (cargo build --release --bin $BIN)…"
  ( cd "$SCRIPT_DIR" && cargo build --release --bin "$BIN" )
  SRC="$SCRIPT_DIR/target/release/$BIN"
fi

info "Installing $BIN → $BIN_DIR/$BIN"
mkdir -p "$BIN_DIR"
cp "$SRC" "$BIN_DIR/$BIN"
chmod 0755 "$BIN_DIR/$BIN"

# PATH hint
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH. Add it, e.g.:"
     echo "    echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc && source ~/.bashrc" ;;
esac

# ---------------------------------------------------------------- Claude Code skill
#
# SKILL.md teaches Claude Code how to drive `ssa`. Installing it is a global change to the
# user's environment (every future session sees it), so it is opt-in and the destination is
# always printed before anything is written.

skill_name() {
  # Read `name:` from the YAML frontmatter rather than hardcoding it, so renaming the skill
  # in SKILL.md does not silently install it under a stale directory.
  local n
  n="$(sed -n '/^---[[:space:]]*$/,/^---[[:space:]]*$/s/^name:[[:space:]]*//p' "$1" | head -1 | tr -d '\r')"
  echo "${n:-analyze-claude-sessions}"
}

install_skill() {
  local src="$1" name dest
  name="$(skill_name "$src")"
  dest="$CLAUDE_DIR/skills/$name"
  info "Installing skill '$name' → $dest/SKILL.md"
  mkdir -p "$dest"
  cp "$src" "$dest/SKILL.md"
  echo "    Claude Code will pick it up in new sessions."
}

SKILL_SRC=""
for c in "$SCRIPT_DIR/SKILL.md" "$SCRIPT_DIR/../SKILL.md"; do
  [[ -f "$c" ]] && { SKILL_SRC="$c"; break; }
done

if [[ "$SKILL" == "no" || -z "$SKILL_SRC" ]]; then
  [[ "$SKILL" == "yes" && -z "$SKILL_SRC" ]] && warn "--skill given but no SKILL.md found next to this script; skipping."
elif [[ "$SKILL" == "yes" ]]; then
  install_skill "$SKILL_SRC"
# Only offer it if Claude Code is actually present — otherwise the directory would be
# created for a tool the user does not have.
elif command -v claude >/dev/null 2>&1 || [[ -d "$CLAUDE_DIR" ]]; then
  # `curl … | bash` leaves stdin pointing at the script, so the answer is read from the
  # terminal directly. /dev/tty being readable is not on its own proof anyone is watching —
  # a CI step can have one — so require a terminal on stdin or stdout too. With neither we
  # never block or nag: skip quietly and name the flag that would have installed it.
  if { [[ -t 0 ]] || [[ -t 1 ]]; } && [[ -r /dev/tty ]]; then
    printf '%b\n' "${BOLD}Claude Code detected.${NC} Install the ssa skill for it?"
    printf '%s\n' "  It teaches Claude Code to analyze session logs with ssa."
    printf '%s\n' "  Writes: $CLAUDE_DIR/skills/$(skill_name "$SKILL_SRC")/SKILL.md"
    printf '%s' "  Install it? [y/N] "
    read -r reply </dev/tty || reply=""
    case "$reply" in
      [yY]|[yY][eE][sS]) install_skill "$SKILL_SRC" ;;
      *) echo "    Skipped. Install later with: $0 --skill" ;;
    esac
  else
    info "Claude Code detected; skipping the skill (no terminal to ask). Use --skill to install it."
  fi
fi

info "Done. Run: $BIN --help"
