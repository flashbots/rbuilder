use alloy_primitives::{BlockHash, U256};
use exponential_backoff::Backoff;
use jsonrpsee::{
    core::{client::ClientT, traits::ToRpcParams},
    http_client::{HttpClient, HttpClientBuilder},
};
use rbuilder::{
    building::BuiltBlockTrace,
    live_builder::{
        block_output::bidding_service_interface::BidObserver, payload_events::MevBoostSlotData,
    },
    utils::error_storage::store_error_event,
};
use rbuilder_primitives::{
    mev_boost::SubmitBlockRequest,
    serialize::{RawBundle, RawShareBundle},
    Bundle, Order,
};
use reth_primitives::SealedBlock;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_with::{serde_as, DisplayFromStr};
use std::{sync::Arc, time::Duration};
use time::format_description::well_known;
use tracing::{error, warn, Span};

use crate::metrics::inc_submit_block_errors;

const BLOCK_PROCESSOR_ERROR_CATEGORY: &str = "block_processor";
const DEFAULT_BLOCK_CONSUME_BUILT_BLOCK_METHOD: &str = "block_consumeBuiltBlockV2";
pub const SIGNED_BLOCK_CONSUME_BUILT_BLOCK_METHOD: &str = "flashbots_consumeBuiltBlockV2";

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

type ConsumeBuiltBlockRequest = (
    BlocksProcessorHeader,
    String,
    String,
    Vec<UsedBundle>,
    Vec<UsedBundle>,
    Vec<UsedSbundle>,
    alloy_rpc_types_beacon::relay::BidTrace,
    String,
    U256,
    U256,
);

/// Struct to avoid copying ConsumeBuiltBlockRequest since HttpClient::request eats the parameter.
#[derive(Clone)]
struct ConsumeBuiltBlockRequestArc {
    inner: Arc<ConsumeBuiltBlockRequest>,
}

impl ConsumeBuiltBlockRequestArc {
    fn new(request: ConsumeBuiltBlockRequest) -> Self {
        Self {
            inner: Arc::new(request),
        }
    }
    fn as_ref(&self) -> &ConsumeBuiltBlockRequest {
        self.inner.as_ref()
    }
}

impl ToRpcParams for ConsumeBuiltBlockRequestArc {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, jsonrpsee::core::Error> {
        let json = serde_json::to_string(self.inner.as_ref())
            .map_err(jsonrpsee::core::Error::ParseError)?;
        RawValue::from_string(json)
            .map(Some)
            .map_err(jsonrpsee::core::Error::ParseError)
    }
}

#[derive(Debug, Clone)]
pub struct BlocksProcessorClient<HttpClientType> {
    client: HttpClientType,
    consume_built_block_method: &'static str,
}

impl BlocksProcessorClient<HttpClient> {
    pub fn try_from(
        url: &str,
        max_request_size: u32,
        max_concurrent_requests: usize,
    ) -> eyre::Result<Self> {
        Ok(Self {
            client: HttpClientBuilder::default()
                .max_request_size(max_request_size)
                .max_concurrent_requests(max_concurrent_requests)
                .build(url)?,
            consume_built_block_method: DEFAULT_BLOCK_CONSUME_BUILT_BLOCK_METHOD,
        })
    }
}

/// RawBundle::encode_no_blobs but more compatible.
fn encode_bundle_for_blocks_processor(mut bundle: Bundle) -> RawBundle {
    // set to 0 when none
    bundle.block = bundle.block.or(Some(0));
    RawBundle::encode_no_blobs(bundle.clone())
}

impl<HttpClientType: ClientT> BlocksProcessorClient<HttpClientType> {
    pub fn new(client: HttpClientType, consume_built_block_method: &'static str) -> Self {
        Self {
            client,
            consume_built_block_method,
        }
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
                        mev_gas_price: res.inplace_sim.full_profit_info().mev_gas_price(),
                        total_eth: res.inplace_sim.full_profit_info().coinbase_profit(),
                        eth_send_to_coinbase: U256::ZERO,
                        total_gas_used: res.inplace_sim.gas_used(),
                        original_bundle: encode_bundle_for_blocks_processor(bundle.clone()),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let used_share_bundles = Self::get_used_sbundles(built_block_trace);

        let params: ConsumeBuiltBlockRequest = (
            header,
            closed_at,
            sealed_at,
            committed_bundles,
            Vec::<UsedBundle>::new(),
            used_share_bundles,
            submit_block_request.bid_trace().clone(),
            builder_name,
            built_block_trace.true_bid_value,
            best_bid_value,
        );
        let request = ConsumeBuiltBlockRequestArc::new(params);
        let backoff = backoff();
        let mut backoff_iter = backoff.iter();
        loop {
            let sleep_time = backoff_iter.next();
            match self
                .client
                .request(self.consume_built_block_method, request.clone())
                .await
            {
                Ok(()) => {
                    return Ok(());
                }
                Err(err) => match sleep_time {
                    Some(time) => {
                        warn!(?err, "Block processor returned error, retrying.");
                        tokio::time::sleep(time).await;
                    }
                    None => {
                        Self::handle_rpc_error(&err, request.as_ref());
                        return Err(err.into());
                    }
                },
            }
        }
    }

    fn handle_rpc_error(err: &jsonrpsee::core::Error, request: &ConsumeBuiltBlockRequest) {
        const RPC_ERROR_TEXT: &str = "Block processor RPC";
        match err {
            jsonrpsee::core::Error::Call(error_object) => {
                error!(err = ?error_object, kind = "error_returned", RPC_ERROR_TEXT);
                store_error_event(BLOCK_PROCESSOR_ERROR_CATEGORY, &err.to_string(), request);
            }
            jsonrpsee::core::Error::Transport(_) => {
                error!(err = ?err, kind = "transport", RPC_ERROR_TEXT);
                store_error_event(BLOCK_PROCESSOR_ERROR_CATEGORY, &err.to_string(), request);
            }
            jsonrpsee::core::Error::ParseError(error) => {
                error!(err = ?err, kind = "deserialize", RPC_ERROR_TEXT);
                let error_txt = error.to_string();
                if !(error_txt.contains("504 Gateway Time-out")
                    || error_txt.contains("502 Bad Gateway"))
                {
                    store_error_event(BLOCK_PROCESSOR_ERROR_CATEGORY, &err.to_string(), request);
                }
            }
            _ => {
                error!(err = ?err, kind = "other", RPC_ERROR_TEXT);
            }
        }
    }

    /// Gets the UsedSbundle carefully considering virtual orders formed by other original orders.
    fn get_used_sbundles(built_block_trace: &BuiltBlockTrace) -> Vec<UsedSbundle> {
        built_block_trace
            .included_orders
            .iter()
            .flat_map(|exec_result| {
                if let Order::ShareBundle(sbundle) = &exec_result.order {
                    // don't like having special cases (merged vs not merged), can we improve this?
                    let filtered_sbundles = if sbundle.is_merged_order() {
                        // We include only original orders that are contained in original_order_ids.
                        // If not contained in original_order_ids then the sub sbundle failed or was an empty execution.
                        sbundle
                            .original_orders
                            .iter()
                            .filter_map(|sub_order| {
                                if let Order::ShareBundle(sbundle) = sub_order {
                                    if exec_result.original_order_ids.contains(&sub_order.id()) {
                                        Some(sbundle)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else if exec_result.tx_infos.is_empty() {
                        // non merged empty execution sbundle
                        vec![]
                    } else {
                        // non merged non empty execution sbundle
                        vec![sbundle]
                    };
                    filtered_sbundles
                        .into_iter()
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

/// BidObserver sending all data to a BlocksProcessorClient
#[derive(Debug)]
pub struct BlocksProcessorClientBidObserver<HttpClientType> {
    client: BlocksProcessorClient<HttpClientType>,
}

impl<HttpClientType> BlocksProcessorClientBidObserver<HttpClientType> {
    pub fn new(client: BlocksProcessorClient<HttpClientType>) -> Self {
        Self { client }
    }
}

impl<HttpClientType: ClientT + Clone + Send + Sync + std::fmt::Debug + 'static> BidObserver
    for BlocksProcessorClientBidObserver<HttpClientType>
{
    fn block_submitted(
        &self,
        _slot_data: &MevBoostSlotData,
        sealed_block: &SealedBlock,
        submit_block_request: &SubmitBlockRequest,
        built_block_trace: &BuiltBlockTrace,
        builder_name: String,
        best_bid_value: U256,
    ) {
        let client = self.client.clone();
        let parent_span = Span::current();
        let sealed_block = sealed_block.clone();
        let submit_block_request = submit_block_request.clone();
        let built_block_trace = built_block_trace.clone();
        tokio::spawn(async move {
            let block_processor_result = client
                .submit_built_block(
                    &sealed_block,
                    &submit_block_request,
                    &built_block_trace,
                    builder_name,
                    best_bid_value,
                )
                .await;
            if let Err(err) = block_processor_result {
                inc_submit_block_errors();
                warn!(parent: &parent_span, ?err, "Failed to submit block to the blocks api");
            }
        });
    }
}

// backoff is around 1 minute and total number of requests per payload will be 4
// assuming 200 blocks per slot and if API is down we will max at around 1k of blocks in memory
fn backoff() -> Backoff {
    let mut backoff = Backoff::new(3, Duration::from_secs(5), None);
    backoff.set_factor(2);
    backoff.set_jitter(0.1);
    backoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_total_time_assert() {
        let mut requests = 0;
        let mut total_sleep_time = Duration::default();
        let backoff = backoff();
        let backoff_iter = backoff.iter();
        for duration in backoff_iter {
            requests += 1;
            total_sleep_time += duration;
        }
        assert_eq!(requests, 4);
        let total_sleep_time = total_sleep_time.as_secs();
        dbg!(total_sleep_time);
        assert!(total_sleep_time > 40 && total_sleep_time < 90);
    }
}
