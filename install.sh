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
  -h, --help        Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) NO_BUILD=true; shift ;;
    --bin-dir)  BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
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

info "Done. Run: $BIN --help"
