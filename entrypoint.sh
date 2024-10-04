#!/bin/bash
set -e

# Run reth in the background if RETH_CMD is set
if [ -n "$RETH_CMD" ]; then
  echo "Running /app/reth with arguments: $RETH_CMD in the background"
  /app/$RETH_CMD &
fi

sleep 5

# Run rbuilder as the main process
if [ -n "$RBUILDER_CONFIG" ]; then
  echo "Running /app/rbuilder with arguments: $RBUILDER_CONFIG"
  exec /app/rbuilder run
else
  echo "RBUILDER_CONFIG is not set. Exiting."
  exit 1
fi
