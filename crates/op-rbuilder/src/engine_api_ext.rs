//! Odyssey rpc logic.
//!
//! `eth_` namespace overrides:
//!
//! - `eth_getProof` will _ONLY_ return the storage proofs _WITHOUT_ an account proof _IF_ targeting
//!   the withdrawal contract. Otherwise, it fallbacks to default behaviour.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatus};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use tracing::trace;

#[rpc(server, client, namespace = "eth")]
pub trait EthApiOverride {
    #[method(name = "newPayloadV3")]
    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus>;
}

/// Implementation of the `eth_` namespace override
#[derive(Debug)]
pub struct EthApiExt<Eth> {
    eth_api: Eth,
}

impl<E> EthApiExt<E> {
    /// Create a new `EthApiExt` module.
    pub const fn new(eth_api: E) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi + Send + Sync + 'static,
{
    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus> {
        panic!("Not implemented");
    }
}
