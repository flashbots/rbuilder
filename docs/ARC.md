# Building blocks for the Arc chain

rbuilder can build blocks for [Arc](https://www.arc.network/) (an EVM L1 built
on Malachite BFT consensus, [arc-node](https://github.com/circlefin/arc-node)).
Support is selected at compile time with the `arc` cargo feature; the
`arc-rbuilder` binary runs rbuilder in-process with an arc-node execution
node.

## How it differs from Ethereum L1

| | Ethereum | Arc |
|---|---|---|
| Slot trigger | CL p2p `payload_attributes` events | `engine_forkchoiceUpdatedV3` from Malachite |
| Block delivery | MEV-Boost relays (bidding, BLS) | `engine_getPayloadV4/V5` response |
| Chain spec | `reth_chainspec::ChainSpec` | `ArcChainSpec` (Zero3–Zero7 hardforks) |
| EVM | `EthEvmConfig` | `ArcEvmConfig` (custom precompiles, SELFDESTRUCT override, EIP-7708 transfer logs) |
| Base fee | EIP-1559 from parent header field | decoded from parent header `extra_data`; next base fee EMA-computed from on-chain ProtocolConfig params and persisted into SystemAccounting contract storage post-block |
| Gas limit | EIP-1559 gradual adjustment | exact value dictated by the ProtocolConfig contract (ADR-0003) |
| Pre-block system calls | EIP-4788 beacon root + EIP-2935 blockhashes | EIP-2935 blockhashes only (Zero5+) |
| Execution requests | EIP-6110/7002/7251 | none (empty `Requests`, hash still present post-Prague) |

The chain selection lives in `crates/rbuilder/src/chain.rs`; the Arc block
building rules in `crates/rbuilder/src/building/arc_support.rs` mirror
arc-node's `ArcBlockExecutor`/`ArcBlockAssembler` so blocks finalize with the
exact same state root and header an arc-node payload builder would produce.

## Architecture (arc-rbuilder)

```
Malachite ──FCU+attrs──▶ engine API ──▶ RbuilderJobGenerator
                                          │        │
                                          │        ├─▶ slot channel ─▶ rbuilder LiveBuilder
                                          │        │     (order pool, simulation, ordering/parallel builders)
                                          │        │            │ best block, finalized
                                          │        │            ▼
                                          │        └─▶ EnginePayloadRegistry cell
                                          ▼                     │
                              Arc payload job (fallback)        │
                                          │                     │
Malachite ◀──getPayload── resolve: rbuilder block if present ◀──┘
                          else fallback block
```

Consensus liveness never depends on rbuilder: if rbuilder produced nothing for
a job, the stock Arc payload builder's block is returned.

## Running

```bash
# requires ../../arc-node checked out next to this repo (path dependencies)
cargo run -p arc-rbuilder -- node \
    --chain arc-testnet \
    --rbuilder.config crates/arc-rbuilder/config-arc-example.toml
```

The binary accepts all arc-node/reth `node` flags (datadir, ports, engine JWT,
etc.). Malachite consensus connects to the engine API exactly as with a stock
arc-node execution node.

## Development notes

* Both repos must pin the same reth release (currently `v1.11.3`); the git
  dependencies unify because both use `tag = "v1.11.3"`.
* `cargo check -p rbuilder` builds the Ethereum flavor, `cargo check -p
  rbuilder --features arc` the Arc flavor. CI should keep both green.
* Precompile call caching is disabled for Arc
  (`ArcCachedEvmFactory`): NativeCoinControl/SystemAccounting precompiles read
  contract storage, so caching by input would return stale results.
