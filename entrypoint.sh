#!/bin/bash
set -e

if [ -n "$RETH_CMD" ]; then
  echo "Running reth with arguments: $RETH_CMD"
  /app/reth $RETH_CMD
  sleep 5
fi

if [ -n "$RBUILDER_CONFIG_PATH" ]; then
  echo "Running rbuilder with config file: $RBUILDER_CONFIG_PATH"
  /app/rbuilder run $RBUILDER_CONFIG_PATH
fi

