# ==============================================================================
# Maya Downloader - POSIX Shell Installer (Linux & macOS)
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/uixatik/maya/main/install.sh | sh
# ==============================================================================

set -e

VERSION="${VERSION:-v0.0.1}"
REPO="${REPO:-uixatik/maya}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

echo "[*] Installing Maya Downloader ($VERSION)..."
mkdir -p "$INSTALL_DIR"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "[-] Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "[*] Detected platform: $OS-$ARCH"

if [ -f "./target/release/maya" ]; then
    echo "[+] Found local build in ./target/release"
    cp ./target/release/maya "$INSTALL_DIR/maya"
    chmod +x "$INSTALL_DIR/maya"
else
    echo "[*] Downloading release from GitHub ($REPO)..."
    ARCHIVE_NAME="maya-$VERSION-$OS-$ARCH.tar.gz"
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/$ARCHIVE_NAME"
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT
    if curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/$ARCHIVE_NAME"; then
        tar -xzf "$TMP_DIR/$ARCHIVE_NAME" -C "$TMP_DIR"
        cp "$TMP_DIR/maya" "$INSTALL_DIR/maya"
        chmod +x "$INSTALL_DIR/maya"
    elif command -v cargo >/dev/null 2>&1; then
        echo "[!] Binary download not found, building from source via Cargo..."
        cargo install --path crates/maya-cli --force
    else
        echo "[-] Failed to download or build Maya binary."
        exit 1
    fi
fi

echo "[+] Maya installed successfully to $INSTALL_DIR/maya"
