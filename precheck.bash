#!/usr/bin/env bash
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
RESET='\033[0m'

pass() { echo -e "${GREEN}✓${RESET} $1"; }
fail() { echo -e "${RED}✗${RESET} $1"; exit 1; }
step() { echo -e "\n${BOLD}▸ $1${RESET}"; }

step "Formatting (cargo +nightly fmt)"
cargo +nightly fmt --all --check || fail "formatting issues found (run 'cargo +nightly fmt --all' to fix)"
pass "All code formatted"

step "Clippy (warnings denied)"
cargo clippy --all-targets --all-features -- -D warnings || fail "clippy warnings found"
pass "No clippy warnings"

step "Check"
cargo check --all-targets --all-features || fail "test targets failed to compile"
pass "Test targets compile"

step "Tests"
cargo test --all-targets --all-features || fail "tests failed"
pass "All tests passed"

echo -e "\n${GREEN}${BOLD}All prechecks passed.${RESET}"
