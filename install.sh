#!/usr/bin/env sh
# install.sh: claude-usage installer
# Usage: curl -fsSL https://raw.githubusercontent.com/abhay/claude-usage-rs/main/install.sh | sh
set -e

REPO="abhay/claude-usage-rs"
BIN="claude-usage"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# ---------------------------------------------------------------------------
# Detect platform
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
      aarch64) TARGET="aarch64-unknown-linux-musl" ;;
      arm64)   TARGET="aarch64-unknown-linux-musl" ;;
      *)       echo "Unsupported Linux arch: $ARCH" >&2; exit 1 ;;
    esac
    EXT="tar.gz"
    ;;
  Darwin)
    case "$ARCH" in
      x86_64) TARGET="x86_64-apple-darwin" ;;
      arm64)  TARGET="aarch64-apple-darwin" ;;
      *)      echo "Unsupported macOS arch: $ARCH" >&2; exit 1 ;;
    esac
    EXT="tar.gz"
    ;;
  *)
    echo "Unsupported OS: $OS (macOS and Linux only)" >&2
    exit 1
    ;;
esac

# ---------------------------------------------------------------------------
# Fetch latest release tag
# ---------------------------------------------------------------------------
VERSION="${VERSION:-$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')}"

if [ -z "$VERSION" ]; then
  echo "Could not determine latest version. Set VERSION env var manually." >&2
  exit 1
fi

FILENAME="${BIN}-${VERSION}-${TARGET}.${EXT}"
URL="https://github.com/$REPO/releases/download/$VERSION/$FILENAME"

echo "Installing $BIN $VERSION for $TARGET..."
echo "From: $URL"

# ---------------------------------------------------------------------------
# Download and install
# ---------------------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -fsSL "$URL" -o "$TMP/$FILENAME"

# Verify checksum if sha256sum is available
if command -v sha256sum >/dev/null 2>&1; then
  CHECKSUMS_URL="https://github.com/$REPO/releases/download/$VERSION/checksums.txt"
  curl -fsSL "$CHECKSUMS_URL" -o "$TMP/checksums.txt"
  cd "$TMP"
  grep "$FILENAME" checksums.txt | sha256sum -c --quiet
  echo "✓ Checksum verified"
  cd -
fi

tar xzf "$TMP/$FILENAME" -C "$TMP"

mkdir -p "$INSTALL_DIR"
EXTRACTED="${BIN}-${VERSION}-${TARGET}"
install -m 755 "$TMP/$EXTRACTED/$BIN" "$INSTALL_DIR/$BIN"

echo "✓ Installed to $INSTALL_DIR/$BIN"

# PATH hint
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "⚠ Add $INSTALL_DIR to your PATH: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

# Initialize config
"$INSTALL_DIR/$BIN" init
echo "Done. Run: $BIN"
