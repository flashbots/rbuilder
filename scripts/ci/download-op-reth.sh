#!/bin/bash

# Download the op-reth release
wget https://github.com/paradigmxyz/reth/releases/download/v1.3.4/op-reth-v1.3.4-x86_64-unknown-linux-gnu.tar.gz

# Extract the tar.gz file
tar -xzf op-reth-v1.3.4-x86_64-unknown-linux-gnu.tar.gz

# Make the binary executable
chmod +x op-reth

# Clean up the tar.gz file (optional)
rm op-reth-v1.3.4-x86_64-unknown-linux-gnu.tar.gz
