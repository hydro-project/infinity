#!/bin/bash
# Install npm dependencies in all lambda subdirectories

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_DIR="$(dirname "$SCRIPT_DIR")"

# Find all directories containing package.json under lib/ (lambda code),
# but skip anything inside node_modules to avoid redundant nested installs.
find "$AGENT_DIR/lib" -path "*/node_modules" -prune -o -name "package.json" -type f -print | while read -r pkg; do
  dir=$(dirname "$pkg")
  echo "Installing dependencies in $dir"
  (cd "$dir" && npm install)
done
