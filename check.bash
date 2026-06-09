#!/usr/bin/env bash
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
RESET='\033[0m'

pass() { echo -e "${GREEN}✓${RESET} $1"; }
fail() { echo -e "${RED}✗${RESET} $1"; exit 1; }
step() { echo -e "\n${BOLD}▸ $1${RESET}"; }

step 'Prerequisites'
command -v git > /dev/null 2>&1 || fail 'Cannot find `git`'
command -v jj > /dev/null 2>&1 || fail 'Cannot find jujutsu `jj`. To install, run `cargo install --locked jj-cli --bin jj`'
command -v bwrap > /dev/null 2>&1 || command -v sandbox-exec > /dev/null 2>&1 || fail 'Cannot find bubblewrap `bwrap` or `sandbox-exec`'
pass 'All prerequisite executables found'

# Set all local crates to DEBUG, keeping any existing RUST_LOG.
export RUST_LOG="${RUST_LOG:-warn},$(cargo tree --workspace --depth 0 | grep -oE '^[a-z][a-z0-9_-]+' | sed 's/-/_/g' | sed 's/$/=debug/' | paste -sd, -)"
export RUST_BACKTRACE=1

step "Formatting"
# Suppress diff output so AI agents run `cargo fmt` instead of manually applying each diff.
cargo fmt --all --check > /dev/null 2>&1 || fail "formatting issues found (run 'cargo fmt --all' to fix)"
pass "All Rust code is formatted"

step "Docs formatting"
if [ -f docs/node_modules/.bin/prettier ]; then
    (cd docs && npx prettier --check "src/**/*.{tsx,ts,css}" "docusaurus.config.ts") || fail "docs formatting issues found (run 'cd docs && npm run format' to fix)"
    pass "All docs code is formatted"
else
    echo "  (skipped: docs node_modules not installed)"
fi

step "infinity-ui formatting"
if [ -f infinity-ui/node_modules/.bin/prettier ]; then
    (cd infinity-ui && npx prettier --check "src/**/*.{tsx,ts,css}") || fail "infinity-ui formatting issues found (run 'cd infinity-ui && npm run format' to fix)"
    pass "All infinity-ui code is formatted"
else
    echo "  (skipped: infinity-ui node_modules not installed)"
fi

step "Clippy (warnings denied)"
cargo clippy --all-targets -- -D warnings || fail "clippy warnings found"
pass "No clippy warnings"

step "Check"
cargo check --all-targets || fail "test targets failed to compile"
pass "Test targets compile"

step "Tests"
cargo test --all-targets || fail "tests failed"
pass "All tests passed"

step "THIRD-PARTY file"
if command -v cargo-about > /dev/null 2>&1; then
    EXPECTED="$(mktemp)"
    cp THIRD-PARTY "$EXPECTED" 2>/dev/null || true
    bash scripts/generate-third-party.sh > /dev/null 2>&1
    if [ -f "$EXPECTED" ] && diff -q "$EXPECTED" THIRD-PARTY > /dev/null 2>&1; then
        pass "THIRD-PARTY file is up to date"
    elif [ ! -f "$EXPECTED" ] || [ ! -s "$EXPECTED" ]; then
        pass "THIRD-PARTY file generated (commit it)"
    else
        fail "THIRD-PARTY file is stale (run 'bash scripts/generate-third-party.sh' and commit)"
    fi
    rm -f "$EXPECTED"
else
    echo "  (skipped: cargo-about not installed)"
fi

echo -e "\n${GREEN}${BOLD}All checks passed.${RESET}"
