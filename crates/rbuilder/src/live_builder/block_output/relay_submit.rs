use crate::{
    building::builders::Block,
    live_builder::{
        block_output::bidding_service_interface::RelaySet, payload_events::MevBoostSlotData,
    },
    mev_boost::{
        sign_block_for_relay, BLSBlockSigner, MevBoostRelayBidSubmitter, RelayError, RelaySlotData,
        SubmitBlockErr,
    },
    telemetry::{
        add_relay_submit_time, add_subsidy_value, inc_conn_relay_errors,
        inc_failed_block_simulations, inc_initiated_submissions, inc_other_relay_errors,
        inc_relay_accepted_submissions, inc_subsidized_blocks, inc_too_many_req_relay_errors,
        mark_submission_start_time,
    },
    utils::{duration_ms, error_storage::store_error_event},
};
use ahash::HashMap;
use alloy_primitives::{utils::format_ether, Address, U256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_rpc_types_engine::ExecutionPayload;
use futures::FutureExt as _;
use mockall::automock;
use parking_lot::Mutex;
use rbuilder_primitives::{
    built_block::{block_to_execution_payload, SignedBuiltBlock},
    mev_boost::{
        BidAdjustmentData, BidMetadata, BidValueMetadata, ExecutionPayloadHeaderElectra,
        HeaderSubmission, HeaderSubmissionElectra, MevBoostRelayID, SignedHeaderSubmission,
        SubmitBlockRequest, SubmitBlockRequestWithMetadata, SubmitHeaderRequest,
        SubmitHeaderRequestWithMetadata, ValidatorSlotData,
    },
};
use reth_chainspec::ChainSpec;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::{
    sync::{broadcast, Notify},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, info_span, trace, warn, Instrument, Span};

use super::bidding_service_interface::BidObserver;

const SIM_ERROR_CATEGORY: &str = "submit_block_simulation";

/// Contains the last pending block so far.
/// Building updates via update while relay submitter polls via take_pending_block.
/// A new block can be waited without polling via wait_for_change.
#[derive(Debug, Default)]
pub struct PendingBlockCell {
    block: Mutex<Option<Block>>,
    block_notify: Notify,
}

impl PendingBlockCell {
    /// Updates unless it's exactly the same block (hash)
    pub fn update(&self, block: Block) {
        let mut current_block = self.block.lock();
        let old_block_hash = current_block
            .as_ref()
            .map(|b| b.sealed_block.hash())
            .unwrap_or_default();
        if block.sealed_block.hash() != old_block_hash {
            *current_block = Some(block);
            self.block_notify.notify_one();
        }
    }

    pub fn take_pending_block(&self) -> Option<Block> {
        self.block.lock().take()
    }

    pub async fn wait_for_change(&self) {
        self.block_notify.notified().await
    }
}

/// Adapts BestBlockCell to BlockBuildingSink by calling compare_and_update on new_block.
#[derive(Debug)]
struct PendingBlockCellToBlockBuildingSink {
    pending_block_cell: Arc<PendingBlockCell>,
}

impl BlockBuildingSink for PendingBlockCellToBlockBuildingSink {
    fn new_block(&self, block: Block) {
        self.pending_block_cell.update(block);
    }
}

/// Sink that can sends a block to a specific relay set.
/// A little ugly add the Relay concept at this level....
#[automock]
pub trait MultiRelayBlockBuildingSink: std::fmt::Debug + Send + Sync {
    fn new_block(&self, relay_set: RelaySet, block: Block);
}

/// Final destination of blocks (eg: submit to the relays).
/// The destination of the blocks is abstracted, can be a single relay or a set of relays.
#[automock]
pub trait BlockBuildingSink: std::fmt::Debug + Send + Sync {
    fn new_block(&self, block: Block);
}

#[derive(Debug)]
pub struct SubmissionConfig {
    pub chain_spec: Arc<ChainSpec>,
    pub signer: BLSBlockSigner,
    pub optimistic_v3_config: Option<OptimisticV3Config>,
    pub bid_observer: Box<dyn BidObserver + Send + Sync>,
}

/// Configuration for optimistic V3.
#[derive(Debug, Clone)]
pub struct OptimisticV3Config {
    /// The URL where the relay can call to retrieve the block.
    pub builder_url: Vec<u8>,
    /// Sender for Optimistic V3 blocks.
    pub block_sender: broadcast::Sender<Arc<AlloySubmitBlockRequest>>,
}

/// Values from [`BuiltBlockTrace`]
struct BuiltBlockInfo {
    pub bid_value: U256,
    pub true_bid_value: U256,
}

/// `run_submit_to_relays_job` is a main function for submitting blocks to relays
///
/// How submission works:
/// 0. We divide relays into optimistic and non-optimistic (defined in config file)
/// 1. We schedule submissions with non-optimistic key for all non-optimistic relays.
///    1.1 If "optimistic_enabled" is false or bid_value >= "optimistic_max_bid_value" we schedule submissions with non-optimistic key
///    returns the best bid made
#[allow(clippy::too_many_arguments)]
async fn run_submit_to_relays_job(
    pending_bid: Arc<PendingBlockCell>,
    slot_data: MevBoostSlotData,
    relays: Vec<MevBoostRelayBidSubmitter>,
    config: Arc<SubmissionConfig>,
    cancel: CancellationToken,
) -> Option<BuiltBlockInfo> {
    let mut res = None;

    let relay_set = RelaySet::new(
        relays
            .iter()
            .map(|relay| relay.id())
            .cloned()
            .collect::<Vec<_>>(),
    );

    let mut last_bid_hash = None;
    'submit: loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(block = slot_data.block(), "run_submit_to_relays_job cancelled");
                break 'submit res;
            },
            _ = pending_bid.wait_for_change() => {}
        };

        let block = if let Some(new_block) = pending_bid.take_pending_block() {
            if last_bid_hash
                .is_none_or(|last_bid_hash| last_bid_hash != new_block.sealed_block.hash())
            {
                last_bid_hash = Some(new_block.sealed_block.hash());
                new_block
            } else {
                continue 'submit;
            }
        } else {
            continue 'submit;
        };

        res = Some(BuiltBlockInfo {
            bid_value: block.trace.bid_value,
            true_bid_value: block.trace.true_bid_value,
        });

        let builder_name = block.builder_name.clone();

        let bundles = block
            .trace
            .included_orders
            .iter()
            .filter(|o| !o.order.is_tx())
            .count();

        // SAFETY: UNIX timestamp in nanos won't exceed u64::MAX until year 2554
        let sequence = OffsetDateTime::now_utc().unix_timestamp_nanos() as u64;
        let executed_orders = block
            .trace
            .included_orders
            .iter()
            .flat_map(|exec_res| exec_res.order.original_orders());
        let bid_metadata = BidMetadata {
            sequence,
            value: BidValueMetadata {
                coinbase_reward: block.trace.coinbase_reward,
                top_competitor_bid: block.trace.seen_competition_bid,
            },
            order_ids: executed_orders.clone().map(|o| o.id()).collect(),
            bundle_hashes: executed_orders
                .filter_map(|o| o.external_bundle_hash())
                .collect(),
        };

        let latency = block.trace.orders_sealed_at - block.trace.orders_closed_at;
        let submission_span = info_span!(
            "bid",
            bid_value = format_ether(block.trace.bid_value),
            true_bid_value = format_ether(block.trace.true_bid_value),
            seen_competition_bid = format_ether(block.trace.seen_competition_bid.unwrap_or_default()),
            block = block.sealed_block.number,
            slot = slot_data.slot(),
            payload_id = slot_data.payload_id,
            hash = ?block.sealed_block.hash(),
            gas = block.sealed_block.gas_used,
            block_id = block.trace.build_block_id.0,
            builder_name = block.builder_name,
            ?relay_set
        );
        info!(
            parent: &submission_span,
            available_orders_statistics = ?block.trace.available_orders_statistics,
            considered_orders_statistics = ?block.trace.considered_orders_statistics,
            failed_orders_statistics = ?block.trace.failed_orders_statistics,
            filtered_build_considered_orders_statistics = ?block.trace.filtered_build_considered_orders_statistics,
            filtered_build_failed_orders_statistics = ?block.trace.filtered_build_failed_orders_statistics,
            bundles,
            txs = block.sealed_block.body().transactions.len(),
            fill_time_ms = duration_ms(block.trace.fill_time),
            finalize_time_ms = duration_ms(block.trace.finalize_time),
            finalize_adjust_time_ms = duration_ms(block.trace.finalize_adjust_time),
            multi_bid_copy_duration_ms = duration_ms(block.trace.multi_bid_copy_duration),
            l1_orders_closed_at = ?block.trace.orders_closed_at,
            l2_chosen_as_best_at = ?block.trace.chosen_as_best_at,
            l3_sent_to_bidder = ?block.trace.sent_to_bidder,
            l4_bid_received_at = ?block.trace.bid_received_at,
            l5_sent_to_sealer = ?block.trace.sent_to_sealer,
            l6_picked_by_sealer_at = ?block.trace.picked_by_sealer_at,
            l7_orders_sealed_at = ?block.trace.orders_sealed_at,
            latency_ms = latency.whole_milliseconds(),
            "Submitting bid",
        );
        inc_initiated_submissions();

        let execution_payload = block_to_execution_payload(
            &config.chain_spec,
            &slot_data.payload_attributes_event.data,
            &block.sealed_block,
        );
        let request = match create_submit_block_request(
            &config.signer,
            &config.chain_spec,
            &slot_data,
            &block,
            &execution_payload,
        ) {
            Ok(request) => request,
            Err(error) => {
                error!(parent: &submission_span, ?error, "Error creating submit block request");
                continue 'submit;
            }
        };

        mark_submission_start_time(block.trace.orders_sealed_at);
        submit_block_to_relays(
            request.clone(),
            &bid_metadata,
            &block.bid_adjustments,
            &relays,
            &slot_data.relay_registrations,
            &config.optimistic_v3_config,
            &submission_span,
            &cancel,
        );

        submission_span.in_scope(|| {
            config.bid_observer.block_submitted(
                &slot_data,
                request,
                Arc::new(block.trace),
                builder_name,
                bid_metadata.value.top_competitor_bid.unwrap_or_default(),
                &relay_set,
            );
        });
    }
}

/// Create submit block request _without_ bid adjustments.
fn create_submit_block_request(
    signer: &BLSBlockSigner,
    chain_spec: &ChainSpec,
    slot_data: &MevBoostSlotData,
    block: &Block,
    execution_payload: &ExecutionPayload,
) -> eyre::Result<Arc<AlloySubmitBlockRequest>> {
    let (message, signature) = sign_block_for_relay(
        signer,
        &block.sealed_block,
        &slot_data.payload_attributes_event.data,
        slot_data.slot_data.pubkey,
        block.trace.bid_value,
    )?;
    SignedBuiltBlock {
        message,
        signature,
        execution_payload: execution_payload.clone(),
        blob_sidecars: block.txs_blobs_sidecars.clone(),
        execution_requests: block.execution_requests.clone(),
    }
    .into_request(chain_spec)
    .map(Arc::new)
}

fn create_optimistic_v3_request(
    builder_url: &[u8],
    request: &AlloySubmitBlockRequest,
    maybe_adjustment_data: Option<&BidAdjustmentData>,
    adjustment_data_required: bool,
) -> eyre::Result<SubmitHeaderRequest> {
    let maybe_adjustment_data_v2 = maybe_adjustment_data.map(|d| d.clone().into_v2());
    if maybe_adjustment_data_v2.is_none() && adjustment_data_required {
        eyre::bail!("adjustment data is required")
    }

    let (submission, tx_count) = match &request {
        AlloySubmitBlockRequest::Electra(request) => {
            let tx_count = request
                .execution_payload
                .payload_inner
                .payload_inner
                .transactions
                .len();
            let submission = SignedHeaderSubmission {
                message: HeaderSubmission::Electra(HeaderSubmissionElectra {
                    bid_trace: request.message.clone(),
                    execution_payload_header: ExecutionPayloadHeaderElectra::from(
                        &request.execution_payload,
                    ),
                    execution_requests: request.execution_requests.clone(),
                    commitments: request.blobs_bundle.commitments.clone(),
                    adjustment_data: maybe_adjustment_data_v2,
                }),
                signature: request.signature,
            };
            (submission, tx_count)
        }
        AlloySubmitBlockRequest::Fulu(request) => {
            let tx_count = request
                .execution_payload
                .payload_inner
                .payload_inner
                .transactions
                .len();
            let submission = SignedHeaderSubmission {
                message: HeaderSubmission::Fulu(HeaderSubmissionElectra {
                    bid_trace: request.message.clone(),
                    execution_payload_header: ExecutionPayloadHeaderElectra::from(
                        &request.execution_payload,
                    ),
                    execution_requests: request.execution_requests.clone(),
                    commitments: request.blobs_bundle.commitments.clone(),
                    adjustment_data: maybe_adjustment_data_v2,
                }),
                signature: request.signature,
            };
            (submission, tx_count)
        }
        _ => eyre::bail!("optimistic v3 submission is not supported for this fork"),
    };

    Ok(SubmitHeaderRequest {
        url: builder_url.to_vec(),
        tx_count: tx_count as u32,
        submission,
    })
}

#[allow(clippy::too_many_arguments)]
fn submit_block_to_relays(
    request: Arc<AlloySubmitBlockRequest>,
    bid_metadata: &BidMetadata,
    bid_adjustments: &std::collections::HashMap<Address, BidAdjustmentData>,
    relays: &Vec<MevBoostRelayBidSubmitter>,
    registrations: &HashMap<MevBoostRelayID, RelaySlotData>,
    optimistic_v3_config: &Option<OptimisticV3Config>,
    submission_span: &Span,
    cancel: &CancellationToken,
) {
    for relay in relays {
        // Blocks go only to relays that have a max bid > bid_value (or no max bid).
        let bid_value = request.bid_trace().value;
        if relay.max_bid().is_some_and(|max| bid_value > max) {
            continue;
        }

        let registration = match registrations.get(relay.id()) {
            Some(registration) => registration.clone(),
            None => {
                // Use any registrations for submitting to test relays.
                debug_assert!(relay.test_relay());
                registrations.values().next().unwrap().clone()
            }
        };

        let maybe_adjustment_data = registration
            .adjustment_fee_payer
            .and_then(|fee_payer| bid_adjustments.get(&fee_payer));

        let mut optimistic_v3 = None;
        if relay.optimistic_v3() {
            if let Some(config) = optimistic_v3_config {
                optimistic_v3 = create_optimistic_v3_request(
                    &config.builder_url,
                    request.as_ref(),
                    maybe_adjustment_data,
                    relay.optimistic_v3_bid_adjustment_required(),
                )
                .map(|request| (config.clone(), SubmitHeaderRequestWithMetadata {
                    submission: request,
                    metadata: bid_metadata.clone()
                }))
                .inspect_err(|error| {
                    error!(parent: submission_span, ?error, "Unable to create optimistic V3 request");
                })
                .ok();
            }
        }

        let submission = SubmitBlockRequestWithMetadata {
            submission: SubmitBlockRequest {
                request: request.clone(),
                // We only set adjustment data on non optimistic v3 submissions.
                // For optimistic v3, it is already included in the header submission.
                adjustment_data: maybe_adjustment_data
                    .filter(|_| optimistic_v3.is_none())
                    .map(|adjustment_data| adjustment_data.clone().into_v1()),
            },
            metadata: bid_metadata.clone(),
        };

        let span = info_span!(parent: submission_span, "relay_submit", relay = &relay.id());
        let relay = relay.clone();
        let cancel = cancel.clone();
        tokio::spawn(
            async move {
                submit_bid_to_the_relay(
                    &relay,
                    submission,
                    optimistic_v3,
                    registration.registration,
                    cancel,
                )
                .await;
            }
            .instrument(span),
        );
    }
}

async fn submit_bid_to_the_relay(
    relay: &MevBoostRelayBidSubmitter,
    submit_block_request: SubmitBlockRequestWithMetadata,
    optimistic_v3_request: Option<(OptimisticV3Config, SubmitHeaderRequestWithMetadata)>,
    registration: ValidatorSlotData,
    cancel: CancellationToken,
) {
    let submit_start = Instant::now();

    if !relay.can_submit_bid() {
        trace!("Relay submission is skipped due to rate limit");
        return;
    };

    let request_fut = if let Some((config, request)) = optimistic_v3_request {
        // Send the block to be saved in cache
        let _ = config
            .block_sender
            .send(submit_block_request.submission.request.clone());
        relay
            .submit_optimistic_v3(request, registration)
            .left_future()
    } else {
        relay
            .submit_block(submit_block_request.clone(), registration)
            .right_future()
    };

    let relay_result = tokio::select! {
        _ = cancel.cancelled() => {
            return;
        },
        res = request_fut => res
    };
    let submit_time = submit_start.elapsed();
    match relay_result {
        Ok(()) => {
            trace!("Block submitted to the relay successfully");
            add_relay_submit_time(relay.id(), submit_time);
            inc_relay_accepted_submissions(relay.id(), relay.optimistic());
        }
        Err(SubmitBlockErr::PayloadDelivered | SubmitBlockErr::PastSlot) => {
            trace!("Block already delivered by the relay, cancelling");
            cancel.cancel();
        }
        Err(SubmitBlockErr::BidBelowFloor | SubmitBlockErr::PayloadAttributesNotKnown) => {
            trace!(
                err = ?relay_result.unwrap_err(),
                "Block not accepted by the relay"
            );
        }
        Err(SubmitBlockErr::SimError(_)) => {
            inc_failed_block_simulations();
            store_error_event(
                SIM_ERROR_CATEGORY,
                relay_result.as_ref().unwrap_err().to_string().as_str(),
                &submit_block_request.submission,
            );
            error!(
                err = ?relay_result.unwrap_err(),
                "Error block simulation fail, cancelling"
            );
            cancel.cancel();
        }
        Err(SubmitBlockErr::RelayError(RelayError::TooManyRequests)) => {
            trace!("Too many requests error submitting block to the relay");
            inc_too_many_req_relay_errors(relay.id());
        }
        Err(SubmitBlockErr::RelayError(RelayError::ConnectionError))
        | Err(SubmitBlockErr::RelayError(RelayError::RequestError(_))) => {
            trace!(err = ?relay_result.unwrap_err(), "Connection error submitting block to the relay");
            inc_conn_relay_errors(relay.id());
        }
        Err(SubmitBlockErr::BlockKnown) => {
            trace!("Block already known");
        }
        Err(SubmitBlockErr::RelayError(_)) => {
            warn!(err = ?relay_result.unwrap_err(), "Error submitting block to the relay");
            inc_other_relay_errors(relay.id());
        }
        Err(SubmitBlockErr::RPCConversionError(_)) => {
            error!(
                err = ?relay_result.unwrap_err(),
                "RPC conversion error (illegal submission?) submitting block to the relay",
            );
        }
        Err(SubmitBlockErr::RPCSerializationError(_)) => {
            error!(
                err = ?relay_result.unwrap_err(),
                "SubmitBlock serialization error submitting block to the relay",
            );
        }
        Err(SubmitBlockErr::InvalidHeader) => {
            error!("Invalid authorization header submitting block to the relay");
        }
        Err(SubmitBlockErr::Grpc(error)) => {
            error!(
                status = ?error.code(),
                err = error.message(),
                "Encountered gRPC error"
            );
        }
        Err(SubmitBlockErr::InvalidUrl(error)) => {
            error!(err = ?error, "Error parsing URL");
        }
        Err(SubmitBlockErr::NoRproxyAvailable) => {} // noop
    }
}

/// Real life MultiRelayBlockBuildingSink that send the blocks to the Relays
#[derive(Debug)]
pub struct RelaySubmitSinkFactory {
    submission_config: Arc<SubmissionConfig>,
    relays: Vec<MevBoostRelayBidSubmitter>,
    /// We expect to get bids only for this specific relay sets.
    relay_sets: Vec<RelaySet>,
}

impl RelaySubmitSinkFactory {
    pub fn new(
        submission_config: SubmissionConfig,
        relays: Vec<MevBoostRelayBidSubmitter>,
        relay_sets: Vec<RelaySet>,
    ) -> Self {
        Self {
            submission_config: Arc::new(submission_config),
            relays,
            relay_sets,
        }
    }

    /// Creates a run_submit_to_relays_job task per RelaySet and returns a RelaySetDispatcher that will submit the blocks to associated task.
    /// Filters out RelaySet with no registered relays.
    pub fn create_builder_sink(
        &self,
        slot_data: MevBoostSlotData,
        cancel: CancellationToken,
    ) -> Box<dyn MultiRelayBlockBuildingSink> {
        // Collect all relays to submit to.
        let mut relays = Vec::new();
        for relay in &self.relays {
            // Only submit to the relays with validator registrations in the slot...
            if slot_data.relay_registrations.contains_key(relay.id())
                // ...and all test relays.
                || relay.test_relay()
            {
                relays.push(relay.clone());
            }
        }
        let mut sinks = HashMap::<_, Box<dyn BlockBuildingSink>>::default();
        for relay_set in &self.relay_sets {
            let relays_in_set = relays
                .iter()
                .filter(|relay| relay_set.relays().contains(relay.id()))
                .cloned()
                .collect::<Vec<_>>();
            if !relays_in_set.is_empty() {
                let pending_block_cell = Arc::new(PendingBlockCell::default());
                // Spawn the task to submit to selected relays and keep track of subsidized blocks.
                tokio::spawn({
                    let pending = pending_block_cell.clone();
                    let config = self.submission_config.clone();
                    let slot_data = slot_data.clone();
                    let cancel = cancel.clone();
                    async move {
                        let last_info = run_submit_to_relays_job(
                            pending,
                            slot_data,
                            relays_in_set,
                            config,
                            cancel,
                        )
                        .await;
                        if let Some(info) = last_info {
                            if info.bid_value > info.true_bid_value {
                                inc_subsidized_blocks(false);
                                add_subsidy_value(info.bid_value - info.true_bid_value, false);
                            }
                        }
                    }
                });
                sinks.insert(
                    relay_set.clone(),
                    Box::new(PendingBlockCellToBlockBuildingSink { pending_block_cell }),
                );
            }
        }
        Box::new(RelaySetDispatcher::new(sinks))
    }
}

/// Implements MultiRelayBlockBuildingSink by dispatching the blocks to the corresponding relay sets.

#[derive(Debug)]
struct RelaySetDispatcher {
    sinks: HashMap<RelaySet, Box<dyn BlockBuildingSink>>,
}

impl RelaySetDispatcher {
    pub fn new(sinks: HashMap<RelaySet, Box<dyn BlockBuildingSink>>) -> Self {
        Self { sinks }
    }
}

impl MultiRelayBlockBuildingSink for RelaySetDispatcher {
    fn new_block(&self, relay_set: RelaySet, block: Block) {
        if let Some(sink) = self.sinks.get(&relay_set) {
            sink.new_block(block);
        } else {
            error!(relay_set = ?relay_set, "Relay set not found");
        }
    }
}
