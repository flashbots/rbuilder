#!/bin/bash
set -e

if [ -n "$RETH_ENV" ]; then
  echo "Running /app/reth with arguments: $RETH_ENV"
  /app/reth $RETH_ENV
fi

# Check if CL_ENDPOINT is set
if [ -n "$CL_ENDPOINT" ]; then
  # Function to check the endpoint
  check_endpoint() {
    echo "Waiting for $CL_ENDPOINT/eth/v1/config/spec to become available..."
    until curl -s -f "$CL_ENDPOINT/eth/v1/config/spec" > /dev/null; do
      echo "Endpoint not available yet. Retrying in 5 seconds..."
      sleep 5
    done
    echo "Endpoint is available."
  }

  # Call the function to check the endpoint
  check_endpoint
else
  echo "CL_ENDPOINT is not set. Skipping endpoint check."
fi

# Proceed with running the binaries if the environment variables are set
if [ -n "$RBUILDER_ENV" ]; then
  echo "Running /app/rbuilder with arguments: $RBUILDER_ENV"
  /app/rbuilder $RBUILDER_ENV
fi


