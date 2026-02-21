#!/usr/bin/env bash
set -euo pipefail

# Bounded builder-playground lifecycle for vbuilder.
# Build -> start playground -> run integration tests -> teardown.
#
# Usage: scripts/playground-check.sh [--no-build] [--no-teardown]
#
# Flags:
#   --no-build     Skip cargo build (use existing debug binary)
#   --no-teardown  Leave playground running after completion (for inspection)
#
# Logs: each run writes to /tmp/playground-runs/<RUN_ID>/
#   build.log      — cargo build output
#   playground.log — builder-playground startup and runtime output
#   test.log       — integration test output

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

POLL_INTERVAL=2   # seconds between health checks
TIMEOUT=60        # seconds to wait for "All services are healthy"

DO_BUILD=true
DO_TEARDOWN=true
PLAYGROUND_PID=""

for arg in "$@"; do
  case "$arg" in
    --no-build)    DO_BUILD=false ;;
    --no-teardown) DO_TEARDOWN=false ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

# Set up run directory
RUN_ID="$(date +%Y%m%d-%H%M%S)"
RUN_DIR="/tmp/playground-runs/$RUN_ID"
mkdir -p "$RUN_DIR"
echo "==> Run ID: $RUN_ID"
echo "==> Logs: $RUN_DIR"

cleanup() {
  if [ -n "$PLAYGROUND_PID" ] && kill -0 "$PLAYGROUND_PID" 2>/dev/null; then
    if [ "$DO_TEARDOWN" = true ]; then
      echo "==> Stopping playground (PID: $PLAYGROUND_PID)..."
      kill "$PLAYGROUND_PID" 2>/dev/null || true
      wait "$PLAYGROUND_PID" 2>/dev/null || true
    else
      echo "==> --no-teardown: playground (PID: $PLAYGROUND_PID) left running"
    fi
  fi
}

trap cleanup EXIT

# Step 1: Verify builder-playground is installed
if ! command -v builder-playground >/dev/null 2>&1; then
  echo "==> ERROR: builder-playground not found in PATH"
  echo "    Install via the flashbots/flashbots-toolchain GitHub Action (CI)"
  echo "    or download a release binary from https://github.com/flashbots/builder-playground"
  exit 1
fi
echo "==> builder-playground: $(command -v builder-playground)"

# Step 2: Build rbuilder
if [ "$DO_BUILD" = true ]; then
  echo "==> Building rbuilder (log: $RUN_DIR/build.log)..."
  cargo build --features="" 2>&1 | tee "$RUN_DIR/build.log"
  echo "==> Build complete"
else
  echo "==> --no-build: skipping cargo build"
fi

# Step 3: Start builder-playground in background
echo "==> Starting builder-playground (log: $RUN_DIR/playground.log)..."
builder-playground start l1 --use-native-reth > "$RUN_DIR/playground.log" 2>&1 &
PLAYGROUND_PID=$!
echo "==> Playground started (PID: $PLAYGROUND_PID)"

# Step 4: Poll for "All services are healthy"
echo "==> Waiting for playground to be healthy (timeout: ${TIMEOUT}s)..."
elapsed=0
while [ "$elapsed" -lt "$TIMEOUT" ]; do
  if grep -q "All services are healthy" "$RUN_DIR/playground.log" 2>/dev/null; then
    echo "==> All services are healthy! (${elapsed}s)"
    break
  fi
  if ! kill -0 "$PLAYGROUND_PID" 2>/dev/null; then
    echo "==> FAIL: playground process died unexpectedly"
    echo "--- last 50 lines of playground.log ---"
    tail -n 50 "$RUN_DIR/playground.log" || true
    exit 1
  fi
  sleep "$POLL_INTERVAL"
  elapsed=$((elapsed + POLL_INTERVAL))
done

if [ "$elapsed" -ge "$TIMEOUT" ]; then
  echo "==> FAIL: Timeout waiting for playground to be healthy (${TIMEOUT}s)"
  echo "--- last 50 lines of playground.log ---"
  tail -n 50 "$RUN_DIR/playground.log" || true
  exit 1
fi

# Step 5: Run integration tests
echo "==> Running integration tests (log: $RUN_DIR/test.log)..."
set +e
PLAYGROUND=TRUE cargo test --features="" -p rbuilder --lib -- integration --test-threads=1 \
  2>&1 | tee "$RUN_DIR/test.log"
TEST_EXIT=${PIPESTATUS[0]}
set -e

# Step 6: Report
if [ "$TEST_EXIT" -ne 0 ]; then
  echo "==> FAIL: integration tests failed (exit $TEST_EXIT)"
  echo "    See $RUN_DIR/test.log for details"
  echo "    Full logs: $RUN_DIR"
  exit 1
fi

echo "==> SUCCESS: all integration tests passed"
echo "==> Full logs: $RUN_DIR"
