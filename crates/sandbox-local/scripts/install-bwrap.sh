#!/usr/bin/env bash
# Build and install bubblewrap (bwrap) from source using only a C compiler.
# No meson/ninja required.
# Usage: ./install-bwrap.sh [--prefix /usr/local]
set -euo pipefail

PREFIX="/usr/local"
BWRAP_VERSION="v0.11.1"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        *) echo "Usage: $0 [--prefix /usr/local]"; exit 1 ;;
    esac
done

BWRAP_BIN="$PREFIX/bin/bwrap"

if command -v "$BWRAP_BIN" &>/dev/null; then
    echo "bwrap already installed at $BWRAP_BIN"
    "$BWRAP_BIN" --version
    exit 0
fi

if ! command -v cc &>/dev/null; then
    echo "Error: C compiler (cc/gcc) not found"
    exit 1
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

echo "Cloning bubblewrap $BWRAP_VERSION..."
git clone --depth 1 --branch "$BWRAP_VERSION" \
    https://github.com/containers/bubblewrap.git "$WORKDIR/bubblewrap"

cd "$WORKDIR/bubblewrap"

# Generate the config header that meson would normally produce.
cat > config.h <<'EOF'
#define PACKAGE_STRING "bubblewrap 0.11.0"
#define ENABLE_REQUIRE_USERNS 0
EOF

# Check for libcap header; build without it if missing.
if ! echo '#include <sys/capability.h>' | cc -E - &>/dev/null 2>&1; then
    echo "Error: libcap headers not found. Install with:"
    echo "  sudo yum install libcap-devel"
    exit 1
fi

echo "Building..."
cc -Wall -O2 -I. -D_GNU_SOURCE -DHAVE_LIBCAP \
    bubblewrap.c bind-mount.c network.c utils.c \
    -o bwrap -lcap

echo "Installing to $PREFIX..."
mkdir -p "$PREFIX/bin"
if [[ -w "$PREFIX/bin" ]]; then
    cp bwrap "$PREFIX/bin/bwrap"
else
    sudo cp bwrap "$PREFIX/bin/bwrap"
fi

echo "Done: $("$BWRAP_BIN" --version)"
