#!/usr/bin/env bash
# Simple test script for ewm-compositor
set -e

SCRIPT_DIR="$(dirname "$0")"
cd "$SCRIPT_DIR/../ewm-compositor"

LOG_FILE="$SCRIPT_DIR/compositor.log"

# Clean up stale processes/sockets
pkill -f ewm-compositor 2>/dev/null || true
rm -f "/run/user/$(id -u)/wayland-ewm"* 2>/dev/null || true

# Clean up on exit
cleanup() {
    kill $COMPOSITOR_PID 2>/dev/null || true
    kill $CLIENT_PID 2>/dev/null || true
    rm -f "/run/user/$(id -u)/wayland-ewm"* 2>/dev/null || true
    echo "Logs saved to: $LOG_FILE"
}
trap cleanup EXIT

# Build
echo "Building..."
cargo build 2>&1 | tee "$LOG_FILE"

# Start compositor
echo "Starting compositor..."
./target/debug/ewm-compositor 2>&1 | tee -a "$LOG_FILE" &
COMPOSITOR_PID=$!
sleep 2

# Start test client (blue background for visibility)
echo "Starting test client..."
WAYLAND_DISPLAY=wayland-ewm foot --override=colors.background=0000ff -e bash -c 'echo "TOP"; sleep 30' &
CLIENT_PID=$!

echo "Running... Press Ctrl+C to stop"
wait $COMPOSITOR_PID
