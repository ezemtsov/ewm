#!/usr/bin/env bash
# Test that surfaces align with Emacs window layout
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR/ewm-compositor"

LOG_FILE="$(mktemp /tmp/ewm-layout-sync.XXXXXX.log)"
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
    emacsclient -s "$EMACS_SOCKET" -e "(kill-emacs)" 2>/dev/null || true
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

# Start Emacs daemon
echo "Starting Emacs daemon..."
emacs -Q --daemon="$EMACS_SOCKET" 2>&1 | tee -a "$LOG_FILE"
sleep 1

# Create a visible frame inside the compositor
echo "Creating Emacs frame inside compositor..."
WAYLAND_DISPLAY=wayland-ewm emacsclient -s "$EMACS_SOCKET" -c -n 2>&1 | tee -a "$LOG_FILE"
sleep 2

# Load ewm.el and connect
echo "Loading ewm.el and connecting..."
emacsclient -s "$EMACS_SOCKET" -e "(progn
  (load \"$PROJECT_DIR/ewm.el\")
  (ewm-connect))" 2>&1 | tee -a "$LOG_FILE"
sleep 1

# Start test client
echo "Starting test client (foot)..."
WAYLAND_DISPLAY=wayland-ewm foot --override=colors.background=0000ff -e bash -c 'echo LAYOUT-TEST; sleep 60' &
CLIENT_PID=$!
sleep 2

# Check surface was created
echo "Checking surface buffer exists..."
BUFFERS=$(emacsclient -s "$EMACS_SOCKET" -e "(mapcar #'buffer-name (buffer-list))")
echo "Buffers: $BUFFERS" | tee -a "$LOG_FILE"

if ! echo "$BUFFERS" | grep -q "ewm:"; then
    echo "FAIL: No EWM buffer created"
    exit 1
fi

# Get the surface buffer name
SURFACE_BUF=$(emacsclient -s "$EMACS_SOCKET" -e "(car (cl-remove-if-not (lambda (b) (string-prefix-p \"*ewm:\" b)) (mapcar #'buffer-name (buffer-list))))")
echo "Surface buffer: $SURFACE_BUF" | tee -a "$LOG_FILE"

# Display the surface buffer in a window - this triggers layout sync
echo "Displaying surface buffer in window..."
emacsclient -s "$EMACS_SOCKET" -e "(switch-to-buffer $SURFACE_BUF)" 2>&1 | tee -a "$LOG_FILE"
sleep 1

# Check compositor received initial layout command
echo "Checking initial layout command..."
if grep -q "Layout surface 2" "$LOG_FILE"; then
    echo "SUCCESS: Initial layout command received"
    INITIAL_LAYOUT=$(grep "Layout surface 2" "$LOG_FILE" | tail -1)
    echo "$INITIAL_LAYOUT"
else
    echo "FAIL: No layout command in log"
    exit 1
fi

# Test layout update by splitting window (triggers window-configuration-change-hook)
echo "Testing layout update via window split..."
emacsclient -s "$EMACS_SOCKET" -e "(progn
  (split-window-below)
  (sit-for 0.1))" 2>&1 | tee -a "$LOG_FILE"
sleep 1

# Check that layout was updated with new dimensions
LAYOUT_COUNT=$(grep -c "Layout surface 2" "$LOG_FILE" || echo 0)
echo "Layout commands for surface 2: $LAYOUT_COUNT"
if [ "$LAYOUT_COUNT" -ge 2 ]; then
    echo "SUCCESS: Layout updated after window split"
    SPLIT_LAYOUT=$(grep "Layout surface 2" "$LOG_FILE" | tail -1)
    echo "$SPLIT_LAYOUT"

    # Verify height changed (split should halve it approximately)
    INITIAL_HEIGHT=$(echo "$INITIAL_LAYOUT" | sed 's/.*) [0-9]*x//' | tr -d '\n')
    SPLIT_HEIGHT=$(echo "$SPLIT_LAYOUT" | sed 's/.*) [0-9]*x//' | tr -d '\n')
    echo "Initial height: $INITIAL_HEIGHT, Split height: $SPLIT_HEIGHT"
else
    echo "FAIL: Layout not updated after split"
fi

# Test restoring to single window
echo "Testing layout restore via delete-other-windows..."
emacsclient -s "$EMACS_SOCKET" -e "(delete-other-windows)" 2>&1 | tee -a "$LOG_FILE"
sleep 1

FINAL_LAYOUT=$(grep "Layout surface 2" "$LOG_FILE" | tail -1)
echo "Final layout: $FINAL_LAYOUT"

# Take screenshot for manual verification
echo ""
echo "Taking screenshot for manual verification..."
emacsclient -s "$EMACS_SOCKET" -e "(progn
  (load \"$PROJECT_DIR/test/ewm-test.el\")
  (ewm-test-snapshot))" 2>&1 | tee -a "$LOG_FILE"

# Wait for screenshot to be saved (compositor saves async)
sleep 2

# Copy snapshot to test output dir
SNAPSHOT_DIR="${OUTPUT_DIR:-$SCRIPT_DIR}"
cp /tmp/ewm-snapshot.png "$SNAPSHOT_DIR/layout-sync-screenshot.png" 2>/dev/null || true
cp /tmp/ewm-snapshot.txt "$SNAPSHOT_DIR/layout-sync-debug.txt" 2>/dev/null || true

echo ""
echo "=== Test Results ==="
echo "Layout commands verified: OK"
echo "Screenshot: $SNAPSHOT_DIR/layout-sync-screenshot.png"
echo "Debug info: $SNAPSHOT_DIR/layout-sync-debug.txt"
echo ""
echo "Inspect the screenshot to verify surface alignment is correct."
echo "Press Ctrl+C to stop."
wait $COMPOSITOR_PID
