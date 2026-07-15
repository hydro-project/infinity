#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT="$ROOT_DIR/THIRD-PARTY"

# Licenses permitted for npm dependencies (includes guessed variants with *)
NPM_ALLOWED="MIT;MIT*;Apache-2.0;Apache*;ISC;BSD-2-Clause;BSD-3-Clause;0BSD;CC0-1.0;Unlicense;BlueOak-1.0.0;MPL-2.0;Custom: https://shiki.style/"

cd "$ROOT_DIR"

echo "Generating Rust third-party notices..."
cargo about generate about.hbs > "$OUTPUT"

echo "Generating npm third-party notices..."
for dir in infinity-ui agent; do
    (cd "$dir" && npm install --ignore-scripts --no-audit --no-fund &>/dev/null)

    # Check for disallowed licenses
    (cd "$dir" && npx --yes license-checker --production --onlyAllow "$NPM_ALLOWED" 2>/dev/null) \
        || { echo "ERROR: $dir has dependencies with disallowed licenses"; exit 1; }

    echo "" >> "$OUTPUT"
    echo "================================================================================" >> "$OUTPUT"
    echo "npm packages from $dir/" >> "$OUTPUT"
    echo "================================================================================" >> "$OUTPUT"
    echo "" >> "$OUTPUT"
    (cd "$dir" && npx --yes license-checker --production --json --customPath "$SCRIPT_DIR/license-checker-format.json" 2>/dev/null) | python3 -c "
import json, sys
data = json.load(sys.stdin)
for pkg, info in sorted(data.items()):
    licenses = info.get('licenses', 'Unknown')
    text = info.get('licenseText', '')
    print(f'{pkg}')
    print(f'  License: {licenses}')
    if text:
        print()
        print(text)
    print()
    print('---')
    print()
" >> "$OUTPUT"
done

echo "Written to $OUTPUT"
