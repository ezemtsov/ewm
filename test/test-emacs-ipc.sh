#!/usr/bin/env bash
# Test Emacs IPC integration with compositor
set -e

SCRIPT_DIR="$(dirname "$0")"
cd "$SCRIPT_DIR/../ewm-compositor"

LOG_FILE="$SCRIPT_DIR/emacs-ipc.log"
EMACS_SOCKET="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ewm-test-emacs"

# Clean up stale processes/sockets
pkill -f ewm-compositor 2>/dev/null || true
pkill -f "emacs.*ewm-test" 2>/dev/null || true
rm -f "/run/user/$(id -u)/wayland-ewm"* 2>/dev/null || true
rm -f /tmp/ewm.sock 2>/dev/null || true
rm -f "$EMACS_SOCKET" 2>/dev/null || true

cleanup() {
    kill $COMPOSITOR_PID 2>/dev/null || true
    kill $CLIENT_PID 2>/dev/null || true
    kill $EMACS_PID 2>/dev/null || true
    rm -f "/run/user/$(id -u)/wayland-ewm"* 2>/dev/null || true
    rm -f /tmp/ewm.sock 2>/dev/null || true
    rm -f "$EMACS_SOCKET" 2>/dev/null || true
    echo "Logs saved to: $LOG_FILE"
}
trap cleanup EXIT

# Build
echo "Building..."
cargo build --quiet

# Start compositor
echo "Starting compositor..."
./target/debug/ewm-compositor 2>&1 | tee "$LOG_FILE" &
COMPOSITOR_PID=$!
sleep 2

# Start clean Emacs daemon (no init.el)
echo "Starting Emacs daemon..."
emacs -Q --daemon="$EMACS_SOCKET" 2>&1 | tee -a "$LOG_FILE"
EMACS_PID=$(pgrep -f "emacs.*$EMACS_SOCKET" | head -1)
sleep 1

# Load ewm.el and connect
echo "Connecting Emacs to compositor..."
emacsclient -s "$EMACS_SOCKET" -e "(progn (load \"$SCRIPT_DIR/../ewm.el\") (ewm-connect))" 2>&1 | tee -a "$LOG_FILE"
sleep 1

# Start test client
echo "Starting test client..."
WAYLAND_DISPLAY=wayland-ewm foot --override=colors.background=0000ff -e bash -c 'echo TEST; sleep 30' &
CLIENT_PID=$!
sleep 2

# Check if Emacs received the surface event
echo "Checking Emacs buffers..."
BUFFERS=$(emacsclient -s "$EMACS_SOCKET" -e "(mapcar #'buffer-name (buffer-list))" 2>&1)
echo "Buffers: $BUFFERS" | tee -a "$LOG_FILE"

if echo "$BUFFERS" | grep -q "ewm:"; then
    echo "SUCCESS: EWM buffer created in Emacs"
else
    echo "PENDING: No EWM buffer yet (check logs)"
fi

echo "Running... Press Ctrl+C to stop"
wait $COMPOSITOR_PID
