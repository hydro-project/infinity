#!/usr/bin/env bash
set -euo pipefail

# Build and (re)start the Infinity Slack bot from an EXPLICIT revision.
#
# Why this exists: rebuilding from whatever happens to be checked out in the
# default workspace silently deploys the wrong code (e.g. plain `main` without
# the Slack-bot feature branches). This script pins the deploy to a named
# jj revision/bookmark, builds it in a dedicated jj workspace (so your working
# copy is never disturbed), performs a clean SIGTERM handover (the bot persists
# its resume state on shutdown and catches the threads up on startup), and
# records what was deployed.
#
# Usage:
#   scripts/run-slack-bot.sh <revision>     # e.g. pr/slack-compact, main, a commit id
#
# The binary is staged to target/debug/infinity-slack-bot (the path the bot has
# historically run from); the previous binary is kept as *.bak.

REV="${1:?usage: run-slack-bot.sh <jj-revision-or-bookmark> (e.g. pr/slack-compact or main)}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WS_NAME="deploy-slack-bot"
WS_DIR="$REPO_ROOT/.infinity/.deploy/$WS_NAME"
LOG_DIR="${HOME}/.infinity"
STDOUT_LOG="$LOG_DIR/slack.stdout.log"

cd "$REPO_ROOT"

command -v jj >/dev/null || { echo "error: jj not found" >&2; exit 1; }

# Resolve the revision up front so typos fail before we touch anything.
COMMIT=$(jj log --ignore-working-copy -r "$REV" --no-graph -T 'commit_id' | head -c 40)
[ -n "$COMMIT" ] || { echo "error: revision '$REV' did not resolve" >&2; exit 1; }
DESC=$(jj log --ignore-working-copy -r "$REV" --no-graph -T 'description.first_line()')
echo "deploying $REV = ${COMMIT:0:12} \"$DESC\""

# Dedicated build workspace, pinned to the requested revision.
if [ ! -d "$WS_DIR" ]; then
    mkdir -p "$(dirname "$WS_DIR")"
    jj workspace add --name "$WS_NAME" "$WS_DIR"
fi
(
    cd "$WS_DIR"
    jj workspace update-stale >/dev/null 2>&1 || true
    jj new "$COMMIT" >/dev/null
    cargo build -p infinity-slack-bot
)

# Stage the binary where the bot runs from.
BIN="$REPO_ROOT/target/debug/infinity-slack-bot"
mkdir -p "$(dirname "$BIN")"
[ -f "$BIN" ] && cp "$BIN" "$BIN.bak"
cp "$WS_DIR/target/debug/infinity-slack-bot" "$BIN.new"
mv "$BIN.new" "$BIN"

# Clean handover: SIGTERM lets the bot drain streams and persist resume state.
OLDPID=$(pgrep -f "target/debug/infinity-slack-bot$" | head -1 || true)
if [ -n "$OLDPID" ]; then
    echo "stopping old bot (pid $OLDPID)..."
    kill -TERM "$OLDPID" || true
    for _ in $(seq 1 15); do
        ps -p "$OLDPID" >/dev/null 2>&1 || break
        sleep 1
    done
    if ps -p "$OLDPID" >/dev/null 2>&1; then
        echo "warn: old bot did not exit in 15s; killing" >&2
        kill -9 "$OLDPID" || true
        sleep 1
    fi
fi

mkdir -p "$LOG_DIR"
cd "$REPO_ROOT"
setsid nohup ./target/debug/infinity-slack-bot >> "$STDOUT_LOG" 2>&1 < /dev/null &
NEWPID=$!
sleep 3
if ! ps -p "$NEWPID" >/dev/null 2>&1; then
    echo "error: bot exited immediately; see $STDOUT_LOG" >&2
    exit 1
fi
echo "$COMMIT $REV $(date -u +%FT%TZ)" >> "$LOG_DIR/slack.deployed"
echo "bot running (pid $NEWPID) from ${COMMIT:0:12}"
