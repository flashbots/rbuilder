//! Compile-time chain selection.
//!
//! rbuilder builds blocks for exactly one chain per binary. By default that is
//! Ethereum; enabling the `arc` cargo feature switches the chain-coupled types
//! (chain spec, EVM configuration, EVM factory) and the chain-specific block
//! building rules to the Arc implementations from arc-node.
//!
//! Code should import [`ChainSpec`]/[`EvmConfig`] from this module instead of
//! naming `reth_chainspec::ChainSpec`/`EthEvmConfig` directly.

use std::sync::Arc;

#[cfg(not(feature = "arc"))]
mod eth {
    use std::sync::Arc;

    pub use reth_chainspec::ChainSpec;
    pub use reth_evm_ethereum::EthEvmConfig as EvmConfig;

    /// Node types used to open a reth database directly (backtest, db tools).
    pub use reth_node_ethereum::EthereumNode as DbNodeTypes;

    pub fn evm_config(chain_spec: Arc<ChainSpec>) -> EvmConfig {
        EvmConfig::new(chain_spec)
    }

    /// Wraps a raw reth `ChainSpec` into the chain spec type of this binary.
    pub fn chain_spec_from_inner(inner: reth_chainspec::ChainSpec) -> ChainSpec {
        inner
    }

    /// Access the raw reth `ChainSpec` underneath.
    pub fn inner_chain_spec(spec: &ChainSpec) -> &reth_chainspec::ChainSpec {
        spec
    }

    /// Parses a chain name or a path to a genesis file.
    pub fn parse_chain_spec(value: &str) -> eyre::Result<Arc<ChainSpec>> {
        reth::chainspec::chain_value_parser(value).map_err(|e| eyre::eyre!(e))
    }

    /// Chain spec used for tests/dummies where any chain spec works.
    pub fn chain_spec_for_testing() -> Arc<ChainSpec> {
        reth_chainspec::MAINNET.clone()
    }

    /// Gas limit for the new block.
    /// On Ethereum the protocol moves the gas limit towards the target by a
    /// bounded step each block.
    pub fn next_block_gas_limit(parent_gas_limit: u64, target: Option<u64>) -> u64 {
        alloy_eips::eip1559::calculate_block_gas_limit(
            parent_gas_limit,
            // This is only for tests, target should always be Some since
            // the protocol does NOT cap the block to ETHEREUM_BLOCK_GAS_LIMIT.
            target.unwrap_or(alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M),
        )
    }
}

#[cfg(feature = "arc")]
mod arc {
    use std::sync::Arc;

    use reth_cli::chainspec::ChainSpecParser as _;

    pub use arc_evm::ArcEvmConfig as EvmConfig;
    pub use arc_execution_config::chainspec::ArcChainSpec as ChainSpec;

    pub fn evm_config(chain_spec: Arc<ChainSpec>) -> EvmConfig {
        EvmConfig::new(reth_evm_ethereum::EthEvmConfig::new_with_evm_factory(
            chain_spec.clone(),
            arc_evm::ArcEvmFactory::new(chain_spec),
        ))
    }

    /// Node types used to open an arc-node reth database directly
    /// (backtest, db tools). Minimal stand-in for arc-node's `ArcNode` so the
    /// rbuilder lib does not have to depend on the full arc node crate.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct DbNodeTypes;

    impl reth_node_api::NodeTypes for DbNodeTypes {
        type Primitives = reth::primitives::EthPrimitives;
        type ChainSpec = ChainSpec;
        type Storage = reth_provider::EthStorage;
        type Payload = reth_node_ethereum::EthEngineTypes;
    }

    /// Wraps a raw reth `ChainSpec` into the chain spec type of this binary.
    pub fn chain_spec_from_inner(inner: reth_chainspec::ChainSpec) -> ChainSpec {
        ChainSpec::new(inner)
    }

    /// Access the raw reth `ChainSpec` underneath.
    pub fn inner_chain_spec(spec: &ChainSpec) -> &reth_chainspec::ChainSpec {
        &spec.inner
    }

    /// Parses an arc chain name ("arc-localdev", "arc-devnet", "arc-testnet",
    /// "arc-mainnet") or a path to a genesis file.
    pub fn parse_chain_spec(value: &str) -> eyre::Result<Arc<ChainSpec>> {
        arc_execution_config::chainspec::ArcChainSpecParser::parse(value)
            .map_err(|e| eyre::eyre!(e))
    }

    /// Chain spec used for tests/dummies where any chain spec works.
    pub fn chain_spec_for_testing() -> Arc<ChainSpec> {
        arc_execution_config::chainspec::LOCAL_DEV.clone()
    }

    /// Gas limit for the new block.
    /// On Arc the gas limit is set exactly to the value dictated by the
    /// on-chain ProtocolConfig contract (ADR-0003); there is no gradual
    /// movement towards a target. The caller provides that value (computed
    /// with [`crate::building::arc_support::expected_block_gas_limit`]); when
    /// absent we keep the parent's gas limit.
    pub fn next_block_gas_limit(parent_gas_limit: u64, target: Option<u64>) -> u64 {
        target.unwrap_or(parent_gas_limit)
    }
}

#[cfg(not(feature = "arc"))]
pub use eth::*;

#[cfg(feature = "arc")]
pub use arc::*;

/// True when this binary builds blocks for Arc.
pub const fn is_arc() -> bool {
    cfg!(feature = "arc")
}

/// Creates the chain-appropriate EVM config. Re-exported helper so call sites
/// can stay chain-agnostic.
pub fn evm_config_for(chain_spec: &Arc<ChainSpec>) -> EvmConfig {
    evm_config(chain_spec.clone())
}
