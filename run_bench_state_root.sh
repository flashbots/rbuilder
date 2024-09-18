#!/bin/bash

for i in {1..100}
do
    echo "Running iteration $i"

    # Run the first command
    ~/reth/target/maxperf/reth stage unwind --datadir=/home/ubuntu/merkle/ --chain=mainnet num-blocks 1
    
    # Check if the first command succeeded
    if [ $? -ne 0 ]; then
        echo "Error in the first command at iteration $i. Exiting."
        exit 1
    fi

    # Run the second command
    ./target/release/debug-bench-cache-warming --config config-backtest-example.toml --rpc-url=https://rpc.flashbots.net
    
    # Check if the second command succeeded
    if [ $? -ne 0 ]; then
        echo "Error in the second command at iteration $i. Exiting."
        exit 1
    fi
done
