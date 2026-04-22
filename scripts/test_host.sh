#!/usr/bin/env bash
# BLE host-side test driver (房主端)
#
# Launches `mlink chat <code>`, waits for a peer to connect, then responds to
# pings with matching pongs for 5 rounds before sending "done" and exiting.
#
# Usage: ./scripts/test_host.sh [room_code]

set -u -o pipefail

ROOM_CODE="${1:-567892}"
MLINK_BIN="${MLINK_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/mlink}"
TIMEOUT_PER_STEP="${TIMEOUT_PER_STEP:-30}"
ROUNDS="${ROUNDS:-5}"

if [[ ! -x "$MLINK_BIN" ]]; then
    echo "FAIL: mlink binary not found at $MLINK_BIN"
    echo "      build first with: cargo build -p mlink-cli"
    exit 1
fi

WORK_DIR="$(mktemp -d -t mlink-host.XXXXXX)"
STDIN_FIFO="$WORK_DIR/stdin.fifo"
STDOUT_LOG="$WORK_DIR/stdout.log"
mkfifo "$STDIN_FIFO"

echo "=== host test: room=$ROOM_CODE rounds=$ROUNDS ==="
echo "=== work dir: $WORK_DIR"

# Keep FIFO writable for the whole run (fd 3). Without this, the fifo closes
# as soon as the first `echo >` returns and mlink sees EOF on stdin.
exec 3>"$STDIN_FIFO"

# Launch mlink with stdin from the FIFO, stdout tee'd to a log we can grep.
"$MLINK_BIN" chat "$ROOM_CODE" <"$STDIN_FIFO" >"$STDOUT_LOG" 2>&1 &
MLINK_PID=$!

cleanup() {
    exec 3>&- 2>/dev/null || true
    if kill -0 "$MLINK_PID" 2>/dev/null; then
        kill -INT "$MLINK_PID" 2>/dev/null || true
        # Give mlink a moment to flush its bye line and release the BLE stack.
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$MLINK_PID" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$MLINK_PID" 2>/dev/null || true
    fi
    wait "$MLINK_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# wait_for_line <pattern> <timeout_seconds>
# Returns 0 when a line matching the ERE pattern is found in the log, 1 on timeout.
wait_for_line() {
    local pattern="$1"
    local timeout="$2"
    local elapsed=0
    while (( elapsed < timeout )); do
        if grep -E -q "$pattern" "$STDOUT_LOG" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$MLINK_PID" 2>/dev/null; then
            echo "FAIL: mlink exited early while waiting for /$pattern/"
            return 1
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done
    echo "FAIL: timeout after ${timeout}s waiting for /$pattern/"
    return 1
}

send_line() {
    echo "$1" >&3
    echo "  >> sent: $1"
}

# Step 1-2: wait for peer connected
echo "[1] waiting for peer connected..."
if ! wait_for_line '\[mlink\] peer connected:' "$TIMEOUT_PER_STEP"; then
    echo "----- log tail -----"
    tail -50 "$STDOUT_LOG"
    echo "RESULT: FAIL"
    exit 1
fi
echo "[1] peer connected OK"

SENT=0
RECEIVED=0

# Step 3-7: ping/pong rounds. Incoming messages render as "[<short_id>] <text>".
# We only care about the content tail, so match "] ping N$".
for i in $(seq 1 "$ROUNDS"); do
    echo "[round $i] waiting for ping $i..."
    if ! wait_for_line "\] ping ${i}\$" "$TIMEOUT_PER_STEP"; then
        echo "----- log tail -----"
        tail -50 "$STDOUT_LOG"
        echo "RESULT: FAIL (missed ping $i) sent=$SENT received=$RECEIVED"
        exit 1
    fi
    RECEIVED=$((RECEIVED + 1))
    echo "[round $i] got ping $i, replying pong $i"
    send_line "pong $i"
    SENT=$((SENT + 1))
done

# Step 8: send done
echo "[final] sending done"
send_line "done"
SENT=$((SENT + 1))

# Step 9: settle — give the wire + BLE stack 3s to flush.
sleep 3

# Report
echo ""
echo "===================="
echo "sent     = $SENT   (expected $((ROUNDS + 1)))"
echo "received = $RECEIVED   (expected $ROUNDS)"
if (( SENT == ROUNDS + 1 )) && (( RECEIVED == ROUNDS )); then
    echo "RESULT: PASS"
    exit 0
else
    echo "RESULT: FAIL (count mismatch)"
    exit 1
fi
