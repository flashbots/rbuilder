# vbuilder Architecture

Development patterns, pipeline walkthrough, and crate map for AI assistants and contributors.

## Overview

vbuilder is a MEV block builder built on top of [Reth](https://github.com/paradigmxyz/reth). It continuously builds blocks for each Ethereum slot, simulates order flow (bundles, transactions), and submits the highest-value block to MEV-Boost relays.

The core orchestrator is `LiveBuilder` in `crates/rbuilder/src/live_builder/mod.rs`.

## Block Building Pipeline

```
MEV-Boost Relay / CL
      │  slot announcement (MevBoostSlotData)
      ▼
payload_events_channel
      │
      ▼  (per slot — BlockBuildingPool::start_block_building)
OrderReplacementManager   ←── OrderPool (RPC + Mempool)
      │  (deduplication, cancellation, sequence ordering)
      ▼
OrdersForBlock (push→pull bridge)
      │
OrderSimulationPool (N concurrent simulation threads)
      │  SimulatedOrderCommand stream (broadcast channel)
      ├──► Building task 1 ──┐
      ├──► Building task 2   ├──► UnfinishedBlockBuildingSinkMuxer (best block wins)
      └──► Building task N ──┘
                                    │
                               BiddingService / SlotBidder
                               (how much to bid for this block)
                                    │
                               BidMaker / BlockSealingBidder
                               (add payout tx + compute root hash)
                                    │
                               RelaySubmitSink ──► MEV-Boost Relay
```

## Core Components

### LiveBuilder

Three pluggable components control the builder's behavior:

- **`blocks_source`** (`SlotSource` trait): source of slot building opportunities. For L1: the CL announces slots via MEV-Boost. For L2: a sequencer generates slots.
- **`builders`** (`Vec<BlockBuildingAlgorithm>`): vector of building strategies. Each runs concurrently, filling blocks with orders to maximize MEV. The muxer picks the best result.
- **`sink_factory`** (`UnfinishedBlockBuildingSinkFactory`): destination for built blocks. For L1: needs bidding and relay submission. For testing: a dummy sink.

### Order Flow

Orders arrive from two sources:
- **RPC**: `eth_sendBundle`, `mev_sendBundle`, `eth_sendRawTransaction`, `eth_cancelBundle`
- **Mempool**: new transactions from reth's transaction pool (direct DB connection)

All go to a channel → `OrderPool` (stores in memory, subscription mechanism for block building tasks).

### Order Processing per Slot

1. `OrderReplacementManager` receives replaceable orders from `OrderPool`. It:
   - Converts cancellations and updates into add/remove operations (building tasks only see adds and removes)
   - Corrects ordering when cancellations arrive out of sequence (uses sequence numbers)
2. `OrdersForBlock` bridges the push-model of `OrderReplacementManager` to the pull-model of simulation
3. `OrderSimulationPool` runs N simulation threads. Output: `SimulatedOrderCommand` stream
4. A broadcast channel multiplexes the simulation stream to all building tasks and the trie prefetcher

### Block Sealing and Bidding

Building tasks produce `BlockBuildingHelper` — filled blocks with a `true_block_value` (max MEV extractable).

`BiddingService` / `SlotBidder` decides how much to bid:
- Bid 0 → maximize our profit (keep all MEV)
- Bid `true_block_value` → zero profit (pass all MEV to proposer)
- Bid > `true_block_value` → subsidized (we lose money)

`BidMaker` seals the winning block: inserts the final payout transaction to the validator, then computes the state root (expensive — only done when we decide to bid).

**Current default** (intentionally safe / not competitive):
- `TrueBlockValueBiddingService` (in `block_output/true_value_bidding_service.rs`): bids 100% of true block value (no profit), with no competitor bid data feed

This must be replaced for a competitive production builder.

## Crate Responsibilities

| Crate | Purpose | Key Types |
|-------|---------|-----------|
| `rbuilder` | Main block builder binary + library | `LiveBuilder`, `OrderPool`, `BlockBuildingPool` |
| `rbuilder-primitives` | Core types shared across crates | `Order`, `Bundle`, `BundleTransaction`, `SimulatedOrder` |
| `rbuilder-utils` | Shared utilities | Numeric helpers, time utilities |
| `rbuilder-config` | Config file parsing and validation | `LiveBuilderConfig`, relay config structs |
| `rbuilder-operator` | Operational tooling | CLI tools for deployment and monitoring |
| `rbuilder-rebalancer` | Bundle rebalancing between relays | `RebalancerConfig` |
| `reth-rbuilder` | Reth node integration (direct DB access) | Reth node extension trait |
| `eth-sparse-mpt` | Sparse Merkle Patricia Trie | `SparseMPT` for state root computation |
| `test-relay` | In-process MEV-Boost relay for tests | `TestRelay` |
| `bid-scraper` | Collect historical relay bid data | `BidScraper` |
| `sysperf` | System performance benchmarks | `SysperfRunner` |
| `metrics_macros` | Prometheus metrics derive macros | `metrics!` macro |
| `test_utils` | Shared test helpers (workspace member) | `TestUtils` |

## Key Traits and Abstractions

| Trait | Location | Purpose |
|-------|----------|---------|
| `BlockBuildingAlgorithm` | `building/builders/mod.rs` | A building strategy (greedy, optimistic, etc.) |
| `BiddingService` | `block_output/bidding_service_interface.rs` | Factory for per-slot bidders |
| `SlotBidder` | `block_output/bidding_service_interface.rs` | Decides how much to bid |
| `BidObserver` | `block_output/bidding_service_interface.rs` | Observes bid submissions |
| `BlockBuildingSink` | `block_output/relay_submit.rs` | Final block destination (relay or test) |
| `MultiRelayBlockBuildingSink` | `block_output/relay_submit.rs` | Multi-relay submission interface |
| `RelaySubmissionPolicy` | `block_output/relay_submit.rs` | Controls relay submission behavior |

Note: `blocks_source` in `LiveBuilder` is a concrete type (`MevBoostSlotDataGenerator`), not a trait.

## Reth Integration

rbuilder accesses reth's MDBX database **directly** (not via JSON-RPC). This is the key performance advantage — direct database reads avoid network round-trips for state access.

This is why `--use-native-reth` is required in builder-playground: reth must run on the host OS (not in Docker) so rbuilder can open the same MDBX database. MDBX databases are platform-specific — a Linux Docker database cannot be opened from a macOS host.

Two deployment modes:
- **Standalone** (`rbuilder` binary): rbuilder runs alongside a separately managed reth node, connecting via its database path
- **Embedded** (`reth-rbuilder`): rbuilder runs as a reth node extension, sharing the same process

## Configuration

Config files are TOML. Full field reference: `docs/CONFIG.md`.

Config structs live in `crates/rbuilder-config/`. The `validate-config` binary validates config files:

```bash
make validate-config          # validates all config-*.toml files in repo root
cargo run --bin validate-config -- --config myconfig.toml
```

Relay endpoints are defined in config only — never hardcoded in source code.
