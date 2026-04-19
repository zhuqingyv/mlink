#!/bin/sh
# mlink installer for macOS / Linux
# Usage: curl -fsSL https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.sh | sh
set -eu

REPO="zhuqingyv/mlink"
INSTALL_DIR="${MLINK_INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="mlink"

# ---- colors ----
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    RESET=$(printf '\033[0m')
else
    RED=""; GREEN=""; YELLOW=""; BOLD=""; DIM=""; RESET=""
fi

info()  { printf '%s%s%s %s\n' "$BOLD" "info:" "$RESET" "$1"; }
warn()  { printf '%s%s%s %s\n' "$YELLOW" "warn:" "$RESET" "$1" >&2; }
err()   { printf '%s%s%s %s\n' "$RED" "error:" "$RESET" "$1" >&2; exit 1; }
ok()    { printf '%s%s%s %s\n' "$GREEN" "✓" "$RESET" "$1"; }

need() {
    command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

# ---- detect downloader ----
if command -v curl >/dev/null 2>&1; then
    DL="curl -fsSL"
    DL_OUT="curl -fsSL -o"
elif command -v wget >/dev/null 2>&1; then
    DL="wget -qO-"
    DL_OUT="wget -qO"
else
    err "need curl or wget"
fi

need tar

# ---- detect sha256 tool ----
if command -v sha256sum >/dev/null 2>&1; then
    SHA_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA_CMD="shasum -a 256"
else
    warn "no sha256sum/shasum found, checksum verification will be skipped"
    SHA_CMD=""
fi

# ---- detect platform ----
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            x86_64)        TARGET="x86_64-apple-darwin" ;;
            *) err "unsupported macOS arch: $ARCH" ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64|amd64)  TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
            *) err "unsupported Linux arch: $ARCH" ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (only macOS and Linux are supported)"
        ;;
esac

info "detected platform: ${BOLD}${TARGET}${RESET}"

# ---- fetch latest tag ----
info "fetching latest release tag..."
API_URL="https://api.github.com/repos/${REPO}/releases/latest"
TAG=$($DL "$API_URL" 2>/dev/null | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/') || true

if [ -z "${TAG:-}" ]; then
    err "could not resolve latest release tag from $API_URL"
fi

ok "latest release: ${BOLD}${TAG}${RESET}"

# ---- download artifact ----
ARCHIVE="mlink-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TARBALL_URL="${BASE_URL}/${ARCHIVE}"
SHA_URL="${TARBALL_URL}.sha256"

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t mlink)
trap 'rm -rf "$TMP"' EXIT INT TERM

info "downloading ${DIM}${TARBALL_URL}${RESET}"
$DL_OUT "$TMP/$ARCHIVE" "$TARBALL_URL" || err "download failed: $TARBALL_URL"

# ---- verify checksum ----
if [ -n "$SHA_CMD" ]; then
    info "downloading checksum..."
    if $DL_OUT "$TMP/$ARCHIVE.sha256" "$SHA_URL" 2>/dev/null; then
        info "verifying checksum..."
        EXPECTED=$(awk '{print $1}' "$TMP/$ARCHIVE.sha256")
        ACTUAL=$(cd "$TMP" && $SHA_CMD "$ARCHIVE" | awk '{print $1}')
        if [ "$EXPECTED" != "$ACTUAL" ]; then
            err "checksum mismatch: expected $EXPECTED, got $ACTUAL"
        fi
        ok "checksum verified"
    else
        warn "no checksum file published, skipping verification"
    fi
fi

# ---- extract ----
info "extracting..."
tar -xzf "$TMP/$ARCHIVE" -C "$TMP" || err "extraction failed"

# binary may be at top level or inside a subdir named after the archive
SRC=""
if [ -f "$TMP/$BIN_NAME" ]; then
    SRC="$TMP/$BIN_NAME"
elif [ -f "$TMP/mlink-${TAG}-${TARGET}/$BIN_NAME" ]; then
    SRC="$TMP/mlink-${TAG}-${TARGET}/$BIN_NAME"
else
    SRC=$(find "$TMP" -maxdepth 3 -type f -name "$BIN_NAME" 2>/dev/null | head -n1)
fi

[ -n "$SRC" ] && [ -f "$SRC" ] || err "could not find '$BIN_NAME' binary inside archive"

# ---- install ----
mkdir -p "$INSTALL_DIR"
DEST="$INSTALL_DIR/$BIN_NAME"
mv "$SRC" "$DEST"
chmod +x "$DEST"
ok "installed to ${BOLD}${DEST}${RESET}"

# ---- PATH check ----
case ":$PATH:" in
    *":$INSTALL_DIR:"*) IN_PATH=1 ;;
    *) IN_PATH=0 ;;
esac

if [ "$IN_PATH" -eq 0 ]; then
    warn "$INSTALL_DIR is not in your PATH"
    case "${SHELL:-}" in
        *zsh)  RC="~/.zshrc" ;;
        *bash) RC="~/.bashrc (or ~/.bash_profile on macOS)" ;;
        *fish) RC="~/.config/fish/config.fish" ;;
        *)     RC="your shell rc file" ;;
    esac
    printf '\n  Add this line to %s:\n\n    %sexport PATH="%s:$PATH"%s\n\n' \
        "$RC" "$BOLD" "$INSTALL_DIR" "$RESET"
fi

# ---- version check ----
if [ "$IN_PATH" -eq 1 ] || [ -x "$DEST" ]; then
    VERSION_OUT=$("$DEST" --version 2>/dev/null || echo "")
    [ -n "$VERSION_OUT" ] && ok "$VERSION_OUT"
fi

# ---- platform-specific notes ----
if [ "$OS" = "Darwin" ]; then
    printf '\n%snote:%s first run of %smlink serve%s will request Bluetooth permission.\n' \
        "$BOLD" "$RESET" "$BOLD" "$RESET"
    printf '      if you miss the prompt: System Settings → Privacy & Security → Bluetooth.\n'
fi

printf '\n%sinstall complete.%s run %smlink --help%s to get started.\n' \
    "$GREEN" "$RESET" "$BOLD" "$RESET"
