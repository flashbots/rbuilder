#!/usr/bin/env bash
#
# Integration test for the ClickHouse backup drain loop.
#
# Starts a local ClickHouse container, runs the integration tests against it,
# and cleans up. Requires Docker.
#
# Usage:
#   ./scripts/test-clickhouse-backup-drain.sh           # run all integration tests
#   ./scripts/test-clickhouse-backup-drain.sh --basic    # skip the outage simulation test
#
set -euo pipefail

CONTAINER_NAME="rbuilder-ch-test"
CH_PORT=8123
CH_IMAGE="clickhouse/clickhouse-server:latest"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
fail()  { echo -e "${RED}[FAIL]${NC}  $*"; exit 1; }

cleanup() {
    info "Cleaning up..."
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}

wait_for_clickhouse() {
    local max_attempts=30
    local attempt=0
    while [ $attempt -lt $max_attempts ]; do
        if curl -sf "http://localhost:${CH_PORT}/ping" >/dev/null 2>&1; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.5
    done
    fail "ClickHouse did not become ready within 15 seconds"
}

# --- Pre-flight checks ---

if ! command -v docker &>/dev/null; then
    fail "Docker is required but not installed"
fi

if ! docker info &>/dev/null 2>&1; then
    fail "Docker daemon is not running"
fi

# --- Start ClickHouse ---

trap cleanup EXIT

info "Pulling ClickHouse image (if needed)..."
docker pull -q "$CH_IMAGE" 2>/dev/null || true

info "Starting ClickHouse container: $CONTAINER_NAME"
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker run -d \
    --name "$CONTAINER_NAME" \
    -p "${CH_PORT}:8123" \
    -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
    -e CLICKHOUSE_PASSWORD="" \
    --ulimit nofile=262144:262144 \
    "$CH_IMAGE" >/dev/null

info "Waiting for ClickHouse to be ready..."
wait_for_clickhouse
info "ClickHouse is ready on port $CH_PORT"

# --- Run unit tests first ---

info "Running unit tests..."
if ! cargo test -p rbuilder-utils --lib clickhouse::backup::tests 2>&1; then
    fail "Unit tests failed"
fi
info "Unit tests passed"

# --- Run integration tests ---

info "Running basic integration test..."
if ! cargo test -p rbuilder-utils --lib -- --ignored integration_drain_to_real_clickhouse 2>&1; then
    fail "Basic integration test failed"
fi

if [ "${1:-}" != "--basic" ]; then
    info "Running outage simulation test..."
    if ! cargo test -p rbuilder-utils --lib -- --ignored integration_drain_survives_outage 2>&1; then
        fail "Outage simulation test failed"
    fi
fi

# --- Verify ClickHouse state ---

info "Verifying ClickHouse data..."

ROW_COUNT=$(curl -sf "http://localhost:${CH_PORT}/" \
    --data-binary "SELECT count() FROM default.test_rows" 2>/dev/null | tr -d '[:space:]')

if [ -z "$ROW_COUNT" ] || [ "$ROW_COUNT" = "0" ]; then
    warn "No rows found in test_rows table (table may have been truncated by last test)"
else
    info "Total rows in test_rows: $ROW_COUNT"
fi

DUP_COUNT=$(curl -sf "http://localhost:${CH_PORT}/" \
    --data-binary "SELECT count() FROM (SELECT value, count() as cnt FROM default.test_rows GROUP BY value HAVING cnt > 1)" 2>/dev/null | tr -d '[:space:]')

if [ "$DUP_COUNT" != "0" ] && [ -n "$DUP_COUNT" ]; then
    fail "Found $DUP_COUNT duplicate values in test_rows!"
else
    info "No duplicate rows found"
fi

# --- Done ---

echo ""
info "All tests passed!"
