#!/usr/bin/env bash
# BLE join-side test driver (加入端)
#
# Launches `mlink join <code> --chat`, waits for peer, drives 5 rounds of
# ping/pong, then waits for the host's "done" before exiting.
#
# Usage: ./scripts/test_join.sh [room_code]

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

WORK_DIR="$(mktemp -d -t mlink-join.XXXXXX)"
STDIN_FIFO="$WORK_DIR/stdin.fifo"
STDOUT_LOG="$WORK_DIR/stdout.log"
mkfifo "$STDIN_FIFO"

echo "=== join test: room=$ROOM_CODE rounds=$ROUNDS ==="
echo "=== work dir: $WORK_DIR"

exec 3>"$STDIN_FIFO"

"$MLINK_BIN" join "$ROOM_CODE" --chat <"$STDIN_FIFO" >"$STDOUT_LOG" 2>&1 &
MLINK_PID=$!

cleanup() {
    exec 3>&- 2>/dev/null || true
    if kill -0 "$MLINK_PID" 2>/dev/null; then
        kill -INT "$MLINK_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$MLINK_PID" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$MLINK_PID" 2>/dev/null || true
    fi
    wait "$MLINK_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

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

# Step 3-8: ping → wait for pong, 5 rounds
for i in $(seq 1 "$ROUNDS"); do
    echo "[round $i] sending ping $i"
    send_line "ping $i"
    SENT=$((SENT + 1))

    echo "[round $i] waiting for pong $i..."
    if ! wait_for_line "\] pong ${i}\$" "$TIMEOUT_PER_STEP"; then
        echo "----- log tail -----"
        tail -50 "$STDOUT_LOG"
        echo "RESULT: FAIL (missed pong $i) sent=$SENT received=$RECEIVED"
        exit 1
    fi
    RECEIVED=$((RECEIVED + 1))
    echo "[round $i] got pong $i"
done

# Step 9: wait for done
echo "[final] waiting for done..."
if ! wait_for_line '\] done$' "$TIMEOUT_PER_STEP"; then
    echo "----- log tail -----"
    tail -50 "$STDOUT_LOG"
    echo "RESULT: FAIL (missed done) sent=$SENT received=$RECEIVED"
    exit 1
fi
RECEIVED=$((RECEIVED + 1))
echo "[final] got done"

echo ""
echo "===================="
echo "sent     = $SENT   (expected $ROUNDS)"
echo "received = $RECEIVED   (expected $((ROUNDS + 1)))"
if (( SENT == ROUNDS )) && (( RECEIVED == ROUNDS + 1 )); then
    echo "RESULT: PASS"
    exit 0
else
    echo "RESULT: FAIL (count mismatch)"
    exit 1
fi
