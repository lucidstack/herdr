#!/usr/bin/env bash
# Build the fork's `main`, install it as the everyday herdr and live-hand off
# every running session to it.
#
# HERDR_DAILY_BIN       install target (default ~/.local/bin/herdr)
# HERDR_DAILY_SESSIONS  comma-separated session names to hand off (default: all running)
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]; then
    echo "daily: switch to main first" >&2
    exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
    echo "daily: commit or stash your changes first" >&2
    exit 1
fi

if [ -f "$root/.local/toolchain-env.sh" ]; then
    # shellcheck source=/dev/null
    source "$root/.local/toolchain-env.sh"
fi
CARGO_INCREMENTAL=0 cargo build --release --locked

dest="${HERDR_DAILY_BIN:-$HOME/.local/bin/herdr}"
mkdir -p "$(dirname "$dest")"
if [ -e "$dest" ]; then
    cp -p "$dest" "$dest.prev"
fi
# Rename into place: overwriting a running binary in place on macOS kills
# the processes mapped from it.
install -m 755 target/release/herdr "$dest.new"
mv -f "$dest.new" "$dest"
echo "daily: installed $dest"

if [ -n "${HERDR_DAILY_SESSIONS:-}" ]; then
    sessions="$(printf '%s' "$HERDR_DAILY_SESSIONS" | tr ',' '\n')"
else
    sessions="$(env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH "$dest" session list --json |
        python3 -c 'import json, sys
for s in json.load(sys.stdin)["sessions"]:
    if s.get("running") is True:
        print(s["name"])')"
fi

failed=0
while IFS= read -r name; do
    [ -n "$name" ] || continue
    if env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH HERDR_SESSION="$name" \
        "$dest" server live-handoff --import-exe "$dest"; then
        echo "daily: handed off $name"
    else
        echo "daily: handoff failed for $name; its old server keeps running" >&2
        failed=1
    fi
done <<<"$sessions"

echo "daily: reattach (detach, then run herdr) to use the new client"
exit "$failed"
