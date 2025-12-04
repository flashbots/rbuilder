# BuilderNet Rebalancer

A small, standalone service that moves funds between BuilderNet accounts to keep operational wallets topped up and tidy. It is designed to initiate rebalancing transfers from a builder’s EOA to a set of allow-listed targets without those transfers being counted as builder profit in coinbase accounting.

## Why this exists?

Operating a BuilderNet builder requires several wallets to be funded. This is especially true in the case of bid adjustments when the fee payer account constantly has decreasing balance and the block value is retained in the builder EOA. The rebalancer automates this by:
- Watching balances for a configured set of tracked accounts.
- Sending EOA-originated transfers when thresholds are crossed.

## Configuration

Sample configuration file:
```toml
rpc_url = "<URL>" # reth RPC URL for subscribing to new blocks and requesting state updates 
builder_url = "<URL>" # builder URL for submitting bundles 
transfer_max_priority_fee_per_gas = "<U256>" # the `max_priority_fee_per_gas` to set on the rebalancing transaction

# Logging configuration
env_filter = "info"
log_color = false
log_json = true

# Rebalancing account
[[account]]
id = "<ACCOUNT ID>" # account ID
secret = "<SECRET>" # Private key or env variable name containing the private key
min_balance = "<U256>"

[[rule]]
description = "" # informational description of the rebalancing rule  
source_id = "<ACCOUNT ID>" # rebalancing account ID referencing an account entry in `accounts`
destination = "<ADDRESS>" # destination target address
type = "fund" # funding type rule 
destination_min_balance = "2500000000000000000" # minimum destination balance. after going below it, the destination account will be topped up
destination_target_balance = "5000000000000000000" # the target balance to top up to

[[rule]]
description = "" # informational description of the rebalancing rule  
source_id = "<ACCOUNT ID>" # rebalancing account ID referencing an account entry in `accounts`
destination = "<ADDRESS>" # destination address
type = "sweep" # sweeping type rule 
source_target_balance = "2500000000000000000" # the target balance that should be remained after sweep
source_max_balance = "5000000000000000000" # maximum source balance. after exceeding it, the source account balance will be swept
```
