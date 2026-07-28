#!/usr/bin/env bash
set -euo pipefail

# session-analyzer remote install script
# Usage:
#   curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash
#   curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s v0.1.0
#   curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s -- --gnu
#   curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s -- --bin-dir /usr/local/bin

REPO="maleus-ai/session-analyzer"

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
Usage: get-ssa.sh [VERSION] [OPTIONS...]

Download and install session-analyzer (the \`ssa\` binary) from GitHub Releases.

Arguments:
  VERSION           Version tag to install (e.g. v0.1.0). Defaults to the latest release.

Options:
  --gnu             Use the glibc (gnu) build instead of static musl (Linux only)
  --bin-dir DIR     Install into DIR (forwarded to install.sh; default ~/.local/bin)

Examples:
  curl -sSfL https://raw.githubusercontent.com/${REPO}/master/get-ssa.sh | bash
  curl -sSfL https://raw.githubusercontent.com/${REPO}/master/get-ssa.sh | bash -s v0.1.0
  curl -sSfL https://raw.githubusercontent.com/${REPO}/master/get-ssa.sh | bash -s -- --bin-dir /usr/local/bin
EOF
}

VERSION=""
USE_GNU=false
INSTALL_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --gnu)     USE_GNU=true; shift ;;
    --bin-dir) INSTALL_ARGS+=(--bin-dir "${2:?--bin-dir needs a value}"); shift 2 ;;
    v*)        if [[ -z "$VERSION" ]]; then VERSION="$1"; else INSTALL_ARGS+=("$1"); fi; shift ;;
    *)         INSTALL_ARGS+=("$1"); shift ;;
  esac
done

detect_target() {
  local arch os libc
  arch="$(uname -m)"; os="$(uname -s)"
  case "$os" in
    Linux)
      if [[ "$USE_GNU" == true ]]; then libc="gnu"; else libc="musl"; fi
      case "$arch" in
        x86_64)          echo "x86_64-unknown-linux-${libc}" ;;
        aarch64|arm64)   echo "aarch64-unknown-linux-${libc}" ;;
        *) error "Unsupported architecture: $arch"; exit 1 ;;
      esac ;;
    Darwin)
      case "$arch" in
        arm64)  echo "aarch64-apple-darwin" ;;
        # No Intel build is published. Say so plainly — otherwise this resolves to a
        # target that simply 404s, and an Apple Silicon binary cannot run here.
        x86_64)
          error "Intel macOS is no longer published. Build from source: cargo build --release"
          exit 1 ;;
        *) error "Unsupported architecture: $arch"; exit 1 ;;
      esac ;;
    *) error "Unsupported OS: $os"; exit 1 ;;
  esac
}

downloader() {
  if command -v curl >/dev/null 2>&1; then echo curl
  elif command -v wget >/dev/null 2>&1; then echo wget
  else error "Neither curl nor wget found."; exit 1; fi
}
download() { # url out
  case "$(downloader)" in
    curl) curl -sSfL -o "$2" "$1" ;;
    wget) wget -q -O "$2" "$1" ;;
  esac
}
resolve_latest() {
  local api="https://api.github.com/repos/${REPO}/releases/latest" resp tag
  case "$(downloader)" in
    curl) resp="$(curl -sSfL "$api")" ;;
    wget) resp="$(wget -q -O - "$api")" ;;
  esac
  tag="$(echo "$resp" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
  [[ -n "$tag" ]] || { error "Could not determine the latest release. See https://github.com/${REPO}/releases"; exit 1; }
  echo "$tag"
}

ARCH="$(detect_target)"
[[ -n "$VERSION" ]] || { info "Resolving latest version…"; VERSION="$(resolve_latest)"; }

TARBALL="ssa-${VERSION}-${ARCH}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"

info "Installing session-analyzer ${VERSION} (${ARCH})"
TMPDIR="$(mktemp -d)"; trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${TARBALL}…"
download "$URL" "${TMPDIR}/${TARBALL}"

info "Extracting…"
tar xzf "${TMPDIR}/${TARBALL}" -C "$TMPDIR"

DIR="${TMPDIR}/ssa-${VERSION}"
[[ -d "$DIR" ]] || { error "Expected ${DIR} not found in tarball"; exit 1; }

chmod +x "${DIR}/install.sh"
"${DIR}/install.sh" --no-build "${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"}"
