#!/bin/bash
# Downloads the jj (jujutsu) binary and packages it as a Lambda layer.
# The layer puts jj in /opt/bin, which is on Lambda's PATH.
set -euo pipefail

JJ_VERSION="0.38.0"
JJ_URL="https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LAYER_DIR="${SCRIPT_DIR}/jj-layer"

rm -rf "${LAYER_DIR}"
mkdir -p "${LAYER_DIR}/bin"

echo "Downloading jj v${JJ_VERSION}..."
curl -fsSL "${JJ_URL}" | tar xz -C "${LAYER_DIR}/bin" jj

chmod +x "${LAYER_DIR}/bin/jj"
echo "jj layer ready at ${LAYER_DIR}"
