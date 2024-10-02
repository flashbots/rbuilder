#!/bin/bash
set -e

# Run reth in the background if RETH_ENV is set
if [ -n "$RETH_ENV" ]; then
  echo "Running /app/reth with arguments: $RETH_ENV in the background"
  /app/reth $RETH_ENV &
fi

# Run rbuilder as the main process
if [ -n "$RBUILDER_CONFIG" ]; then
  echo "Running /app/rbuilder with arguments: $RBUILDER_CONFIG"
  exec /app/rbuilder 
else
  echo "RBUILDER_CONFIG is not set. Exiting."
  exit 1
fi

