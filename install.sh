#!/usr/bin/env sh
# ==============================================================================
# Maya Downloader - Linux & macOS POSIX Installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/uixatik/maya/main/install.sh | sh
#   or run locally: ./install.sh
# ==============================================================================

set -eu

VERSION="${VERSION:-v0.0.1}"
REPO="${REPO:-uixatik/maya}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# ANSI Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

info() {
    printf "${CYAN}[*] %s${NC}\n" "$1"
}

success() {
    printf "${GREEN}[+] %s${NC}\n" "$1"
}

warn() {
    printf "${YELLOW}[!] %s${NC}\n" "$1"
}

error() {
    printf "${RED}[-] %s${NC}\n" "$1" >&2
}

printf "${CYAN}"
cat << 'EOF'
============================================================
   __  __                  ____                      _                 _ 
  |  \/  | __ _ _   _  __ |  _ \  _____      ___ __ | | ___   __ _  __| |
  | |\/| |/ _` | | | |/ _`| | | |/ _ \ \ /\ / / '_ \| |/ _ \ / _` |/ _` |
  | |  | | (_| | |_| | (_| | |_| | (_) \ V  V /| | | | | (_) | (_| | (_| |
  |_|  |_|\__,_|\__, |\__,_|____/ \___/ \_/\_/ |_| |_|_|\___/ \__,_|\__,_|
                |___/                                                    
============================================================
Fast, Resilient Multi-Protocol Downloader & Daemon
EOF
printf "${NC}\n"

mkdir -p "$INSTALL_DIR"
BINARIES="maya mayad maya-nmh"
SOURCE_DIR=""

# Detect OS and Architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux*)     PLATFORM="linux" ;;
    Darwin*)    PLATFORM="macos" ;;
    *)          PLATFORM="unknown" ;;
esac

case "$ARCH" in
    x86_64|amd64)   TARGET_ARCH="x86_64" ;;
    aarch64|arm64)  TARGET_ARCH="aarch64" ;;
    *)              TARGET_ARCH="$ARCH" ;;
esac

info "Detected Platform: $PLATFORM ($TARGET_ARCH)"
info "Target Install Directory: $INSTALL_DIR"

# 1. Check local directories first
SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo "")"
LOCAL_CANDIDATES="
$SCRIPT_DIR
$SCRIPT_DIR/dist/maya-$VERSION-$TARGET_ARCH
$SCRIPT_DIR/target/release
./target/release
.
"

for cand in $LOCAL_CANDIDATES; do
    if [ -n "$cand" ] && [ -d "$cand" ]; then
        all_found=true
        for bin in $BINARIES; do
            if [ ! -f "$cand/$bin" ]; then
                all_found=false
                break
            fi
        done
        if [ "$all_found" = true ]; then
            SOURCE_DIR="$cand"
            success "Found local binaries in: $SOURCE_DIR"
            break
        fi
    fi
done

# 2. Download from GitHub Releases if not found locally
if [ -z "$SOURCE_DIR" ]; then
    info "Local binaries not found. Downloading release $VERSION from GitHub..."

    if [ "$PLATFORM" = "linux" ]; then
        RELEASE_TAG="x86_64-unknown-linux-gnu"
    elif [ "$PLATFORM" = "macos" ]; then
        if [ "$TARGET_ARCH" = "aarch64" ]; then
            RELEASE_TAG="aarch64-apple-darwin"
        else
            RELEASE_TAG="x86_64-apple-darwin"
        fi
    else
        RELEASE_TAG="unknown"
    fi

    ARCHIVE_NAME="maya-$VERSION-$RELEASE_TAG.tar.gz"
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/$ARCHIVE_NAME"
    TEMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'maya-install')"
    TEMP_TAR="$TEMP_DIR/$ARCHIVE_NAME"

    download_success=false
    if command -v curl >/dev/null 2>&1; then
        info "Downloading: $DOWNLOAD_URL via curl..."
        if curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_TAR"; then
            download_success=true
        fi
    elif command -v wget >/dev/null 2>&1; then
        info "Downloading: $DOWNLOAD_URL via wget..."
        if wget -q "$DOWNLOAD_URL" -O "$TEMP_TAR"; then
            download_success=true
        fi
    fi

    if [ "$download_success" = true ] && [ -f "$TEMP_TAR" ]; then
        tar -xzf "$TEMP_TAR" -C "$TEMP_DIR"
        if [ -f "$TEMP_DIR/$bin" ]; then
            SOURCE_DIR="$TEMP_DIR"
        elif [ -d "$TEMP_DIR/maya-$VERSION-$RELEASE_TAG" ]; then
            SOURCE_DIR="$TEMP_DIR/maya-$VERSION-$RELEASE_TAG"
        else
            # Search for maya binary in extracted directory
            FOUND="$(find "$TEMP_DIR" -name "maya" -type f | head -n 1)"
            if [ -n "$FOUND" ]; then
                SOURCE_DIR="$(dirname "$FOUND")"
            fi
        fi
    else
        warn "Could not download pre-built binary. Checking if cargo is available to build from source..."
        if command -v cargo >/dev/null 2>&1; then
            info "Building from source with cargo..."
            cargo build --release -p maya-cli -p maya-daemon -p maya-nmh
            if [ -f "./target/release/maya" ]; then
                SOURCE_DIR="./target/release"
            fi
        fi
    fi
fi

if [ -z "$SOURCE_DIR" ]; then
    error "Could not install Maya. Ensure pre-built binaries exist or install Rust (https://rustup.rs)."
    exit 1
fi

# 3. Copy binaries to install directory
info "Installing binaries into $INSTALL_DIR..."
for bin in $BINARIES; do
    if [ -f "$SOURCE_DIR/$bin" ]; then
        cp -f "$SOURCE_DIR/$bin" "$INSTALL_DIR/$bin"
        chmod +x "$INSTALL_DIR/$bin"
        success "Installed $bin"
    else
        warn "Binary not found: $SOURCE_DIR/$bin"
    fi
done

# Copy browser extension files if available
EXT_DEST="$HOME/.local/share/maya/extensions/browser-capture"
if [ -d "$SCRIPT_DIR/extensions/browser-capture" ]; then
    mkdir -p "$EXT_DEST"
    cp -rf "$SCRIPT_DIR/extensions/browser-capture/"* "$EXT_DEST/" 2>/dev/null || true
    success "Installed browser extension files to $EXT_DEST"
fi

# 4. Check PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        warn "$INSTALL_DIR is not in your PATH."
        info "Add the following line to your ~/.bashrc, ~/.zshrc, or ~/.profile:"
        printf "    export PATH=\"%s:\$PATH\"\n" "$INSTALL_DIR"
        ;;
esac

# 5. Register Native Messaging Host manifests with browsers
if [ -f "$INSTALL_DIR/maya-nmh" ]; then
    info "Registering Native Messaging Host manifests with Chrome, Edge, and Firefox..."
    "$INSTALL_DIR/maya-nmh" --install-manifest || warn "Manifest registration produced a warning."
fi

# 6. Verify Installation
printf "\n"
info "Verifying installation..."
if [ -f "$INSTALL_DIR/maya" ]; then
    "$INSTALL_DIR/maya" --version || true
fi

cat << EOF

============================================================
 Maya Downloader Installation Complete!
============================================================
 Binaries installed:
   - $INSTALL_DIR/maya
   - $INSTALL_DIR/mayad
   - $INSTALL_DIR/maya-nmh

 Quick Start:
   maya --help
   maya get "https://example.com/file.zip"
   maya tui
   maya daemon start

 Browser Extension:
   1. Open chrome://extensions or edge://extensions
   2. Enable "Developer mode"
   3. Click "Load unpacked" and select:
      $EXT_DEST
============================================================
EOF
