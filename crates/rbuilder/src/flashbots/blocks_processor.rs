use crate::{
    building::BuiltBlockTrace,
    mev_boost::SubmitBlockRequest,
    primitives::{
        serialize::{RawBundle, RawShareBundle},
        Order,
    },
    utils::{error_storage::store_error_event, http_provider, BoxedProvider},
};
use alloy_json_rpc::RpcError;
use alloy_primitives::{BlockHash, U256};
use alloy_provider::Provider;
use reth::primitives::SealedBlock;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::sync::Arc;
use time::format_description::well_known;
use tracing::{debug, error};

const BLOCK_PROCESSOR_ERROR_CATEGORY: &str = "block_processor";

#[derive(Debug, Clone)]
pub struct BlocksProcessorClient {
    client: Arc<BoxedProvider>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsedSbundle {
    bundle: RawShareBundle,
    success: bool,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsedBundle {
    #[serde_as(as = "DisplayFromStr")]
    mev_gas_price: U256,
    #[serde_as(as = "DisplayFromStr")]
    total_eth: U256,
    #[serde_as(as = "DisplayFromStr")]
    eth_send_to_coinbase: U256,
    #[serde_as(as = "DisplayFromStr")]
    total_gas_used: u64,
    original_bundle: RawBundle,
}

/// Header used by block_consumeBuiltBlockV2. Since docs are not up to date I copied RbuilderHeader from block-processor/ports/models.go (commit b341b35)
/// Based on alloy_primitives::Block
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "camelCase")]
struct BlocksProcessorHeader {
    pub hash: BlockHash,
    pub gas_limit: U256,
    pub gas_used: U256,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_fee_per_gas: Option<U256>,
    pub parent_hash: BlockHash,
    pub timestamp: U256,
    pub number: Option<U256>,
}

impl BlocksProcessorClient {
    pub fn try_from(url: &str) -> eyre::Result<Self> {
        Ok(Self {
            client: Arc::new(http_provider(url.parse()?)),
        })
    }

    pub async fn submit_built_block(
        &self,
        sealed_block: &SealedBlock,
        submit_block_request: &SubmitBlockRequest,
        built_block_trace: &BuiltBlockTrace,
        builder_name: String,
        best_bid_value: U256,
    ) -> eyre::Result<()> {
        let header = BlocksProcessorHeader {
            hash: sealed_block.hash(),
            gas_limit: U256::from(sealed_block.gas_limit),
            gas_used: U256::from(sealed_block.gas_used),
            base_fee_per_gas: sealed_block.base_fee_per_gas.map(U256::from),
            parent_hash: sealed_block.parent_hash,
            timestamp: U256::from(sealed_block.timestamp),
            number: Some(U256::from(sealed_block.number)),
        };
        let closed_at = built_block_trace
            .orders_closed_at
            .format(&well_known::Iso8601::DEFAULT)?;
        let sealed_at = built_block_trace
            .orders_sealed_at
            .format(&well_known::Iso8601::DEFAULT)?;

        let committed_bundles = built_block_trace
            .included_orders
            .iter()
            .filter_map(|res| {
                if let Order::Bundle(bundle) = &res.order {
                    Some(UsedBundle {
                        mev_gas_price: res.inplace_sim.mev_gas_price,
                        total_eth: res.inplace_sim.coinbase_profit,
                        eth_send_to_coinbase: U256::ZERO,
                        total_gas_used: res.inplace_sim.gas_used,
                        original_bundle: RawBundle::encode_no_blobs(bundle.clone()),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let used_share_bundles = Self::get_used_sbundles(built_block_trace);

        let params = (
            header,
            closed_at,
            sealed_at,
            committed_bundles.clone(),
            committed_bundles,
            used_share_bundles,
            submit_block_request.bid_trace(),
            builder_name,
            built_block_trace.true_bid_value,
            best_bid_value,
        );

        match self
            .client
            .raw_request("block_consumeBuiltBlockV2".into(), &params)
            .await
        {
            Ok(()) => {}
            Err(err) => {
                match &err {
                    RpcError::ErrorResp(err) => {
                        error!(err = ?err, "Block processor returned error");
                        store_error_event(
                            BLOCK_PROCESSOR_ERROR_CATEGORY,
                            &err.to_string(),
                            &params,
                        );
                    }
                    RpcError::SerError(err) => {
                        error!(err = ?err, "Failed to serialize block processor request");
                    }
                    RpcError::DeserError { err, text } => {
                        if !(text.contains("504 Gateway Time-out")
                            || text.contains("502 Bad Gateway"))
                        {
                            error!(err = ?err, "Failed to deserialize block processor response");
                            store_error_event(
                                BLOCK_PROCESSOR_ERROR_CATEGORY,
                                &err.to_string(),
                                &params,
                            );
                        }
                    }
                    RpcError::Transport(err) => {
                        debug!(err = ?err, "Failed to send block processor request");
                    }
                    RpcError::NullResp => {
                        error!("Block processor returned null response");
                    }
                    RpcError::UnsupportedFeature(err) => {
                        error!(err = ?err, "Unsupported feature");
                    }
                    RpcError::LocalUsageError(err) => {
                        error!(err = ?err, "Local usage error");
                    }
                }
                return Err(err.into());
            }
        }

        Ok(())
    }

    /// Gets the UsedSbundle carefully considering virtual orders formed by other original orders.
    fn get_used_sbundles(built_block_trace: &BuiltBlockTrace) -> Vec<UsedSbundle> {
        built_block_trace
            .included_orders
            .iter()
            .flat_map(|sim| {
                if let Order::ShareBundle(_) = &sim.order {
                    // get original orders (in case of order merging)
                    sim.order
                        .original_orders()
                        .iter()
                        .filter_map(|sub_order| {
                            if let Order::ShareBundle(sbundle) = sub_order {
                                Some(sbundle)
                            } else {
                                None
                            }
                        })
                        .map(|sbundle| UsedSbundle {
                            bundle: RawShareBundle::encode_no_blobs(sbundle.clone()),
                            success: true,
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            })
            .collect::<Vec<_>>()
    }
}
