use crate::{
    building::builders::Block,
    live_builder::payload_events::MevBoostSlotData,
    mev_boost::{
        sign_block_for_relay,
        submission::{BidMetadata, BidValueMetadata, SubmitBlockRequestWithMetadata},
        BLSBlockSigner, RelayError, SubmitBlockErr,
    },
    primitives::mev_boost::{MevBoostRelayBidSubmitter, MevBoostRelayID},
    telemetry::{
        add_relay_submit_time, add_subsidy_value, inc_conn_relay_errors,
        inc_failed_block_simulations, inc_initiated_submissions, inc_other_relay_errors,
        inc_relay_accepted_submissions, inc_subsidized_blocks, inc_too_many_req_relay_errors,
        mark_submission_start_time,
    },
    utils::error_storage::store_error_event,
};
use ahash::HashMap;
use alloy_primitives::{utils::format_ether, U256};
use mockall::automock;
use parking_lot::Mutex;
use reth_chainspec::ChainSpec;
use std::sync::Arc;
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info_span, trace, warn, Instrument};

use super::{
    bid_observer::BidObserver,
    bid_value_source::{best_bid_sync_source::BestBidSyncSource, interfaces::BidValueSource},
};

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

/// Final destination of blocks (eg: submit to the relays).
#[automock]
pub trait BlockBuildingSink: std::fmt::Debug + Send + Sync {
    fn new_block(&self, block: Block);
}

/// Factory used to create BlockBuildingSink..
pub trait BuilderSinkFactory: std::fmt::Debug + Send + Sync {
    /// # Arguments
    /// slot_bidder: Not always needed but simplifies the design.
    fn create_builder_sink(
        &self,
        slot_data: MevBoostSlotData,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        cancel: CancellationToken,
    ) -> Box<dyn BlockBuildingSink>;
}

#[derive(Debug)]
pub struct SubmissionConfig {
    pub chain_spec: Arc<ChainSpec>,
    pub signer: BLSBlockSigner,

    pub optimistic_config: Option<OptimisticConfig>,
    pub bid_observer: Box<dyn BidObserver + Send + Sync>,
}

/// Configuration for optimistic block submission to relays.
#[derive(Debug, Clone)]
pub struct OptimisticConfig {
    pub signer: BLSBlockSigner,
    pub max_bid_value: U256,
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
    competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
) -> Option<BuiltBlockInfo> {
    let best_bid_sync_source = BestBidSyncSource::new(
        competition_bid_value_source,
        slot_data.block(),
        slot_data.slot(),
    );
    let mut res = None;

    let (normal_relays, optimistic_relays) = {
        let mut normal_relays = Vec::new();
        let mut optimistic_relays = Vec::new();
        for relay in relays {
            if relay.optimistic() {
                optimistic_relays.push(relay);
            } else {
                normal_relays.push(relay);
            }
        }
        (normal_relays, optimistic_relays)
    };

    let mut last_bid_hash = None;
    'submit: loop {
        if cancel.is_cancelled() {
            break 'submit res;
        }

        pending_bid.wait_for_change().await;
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

        // Only enable the optimistic config for this block if the bid value is below the max bid value
        let optimistic_config = config
            .optimistic_config
            .as_ref()
            .and_then(|optimistic_config| {
                if block.trace.bid_value < optimistic_config.max_bid_value {
                    Some(optimistic_config)
                } else {
                    None
                }
            });

        let bid_metadata = BidMetadata {
            value: BidValueMetadata {
                coinbase_reward: block.trace.coinbase_reward,
                top_competitor_bid: block.trace.seen_competition_bid,
            },
        };

        let best_bid_value = best_bid_sync_source.best_bid_value();
        let submission_span = info_span!(
            "bid",
            bid_value = format_ether(block.trace.bid_value),
            best_bid_value = format_ether(best_bid_value.unwrap_or_default()),
            true_bid_value = format_ether(block.trace.true_bid_value),
            seen_competition_bid = format_ether(block.trace.seen_competition_bid.unwrap_or_default()),
            block = block.sealed_block.number,
            hash = ?block.sealed_block.header.hash(),
            gas = block.sealed_block.gas_used,
            txs = block.sealed_block.body().transactions.len(),
            bundles,
            builder_name = block.builder_name,
            fill_time_ms = block.trace.fill_time.as_millis(),
            finalize_time_ms = block.trace.finalize_time.as_millis(),
        );
        debug!(
            parent: &submission_span,
            "Submitting bid",
        );
        inc_initiated_submissions(optimistic_config.is_some());

        let (normal_signed_submission, optimistic_signed_submission) = {
            let normal_signed_submission = match sign_block_for_relay(
                &config.signer,
                &block.sealed_block,
                &block.txs_blobs_sidecars,
                &block.execution_requests,
                &config.chain_spec,
                &slot_data.payload_attributes_event.data,
                slot_data.slot_data.pubkey,
                block.trace.bid_value,
            ) {
                Ok(res) => SubmitBlockRequestWithMetadata {
                    submission: res,
                    metadata: bid_metadata.clone(),
                },
                Err(err) => {
                    error!(parent: &submission_span, err = ?err, "Error signing block for relay");
                    continue 'submit;
                }
            };

            let optimistic_signed_submission = if let Some(optimistic_config) = optimistic_config {
                match sign_block_for_relay(
                    &optimistic_config.signer,
                    &block.sealed_block,
                    &block.txs_blobs_sidecars,
                    &block.execution_requests,
                    &config.chain_spec,
                    &slot_data.payload_attributes_event.data,
                    slot_data.slot_data.pubkey,
                    block.trace.bid_value,
                ) {
                    Ok(res) => Some((
                        SubmitBlockRequestWithMetadata {
                            submission: res,
                            metadata: bid_metadata.clone(),
                        },
                        optimistic_config,
                    )),
                    Err(err) => {
                        error!(parent: &submission_span, err = ?err, "Error signing block for relay");
                        continue 'submit;
                    }
                }
            } else {
                None
            };

            (normal_signed_submission, optimistic_signed_submission)
        };

        mark_submission_start_time(block.trace.orders_sealed_at);

        for relay in &normal_relays {
            let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id(), optimistic = false);
            let relay = relay.clone();
            let cancel = cancel.clone();
            let submission = normal_signed_submission.clone();
            tokio::spawn(
                async move {
                    submit_bid_to_the_relay(&relay, cancel.clone(), submission, false).await;
                }
                .instrument(span),
            );
        }

        if let Some((optimistic_signed_submission, _)) = &optimistic_signed_submission {
            for relay in &optimistic_relays {
                let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id(), optimistic = true);
                let relay = relay.clone();
                let cancel = cancel.clone();
                let submission = optimistic_signed_submission.clone();
                tokio::spawn(
                    async move {
                        submit_bid_to_the_relay(&relay, cancel.clone(), submission, true).await;
                    }
                    .instrument(span),
                );
            }
        } else {
            // non-optimistic submission to optimistic relays
            for relay in &optimistic_relays {
                let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id(), optimistic = false);
                let relay = relay.clone();
                let cancel = cancel.clone();
                let submission = normal_signed_submission.clone();
                tokio::spawn(
                    async move {
                        submit_bid_to_the_relay(&relay, cancel.clone(), submission, false).await;
                    }
                    .instrument(span),
                );
            }
        }

        submission_span.in_scope(|| {
            // NOTE: we only notify normal submission here because they have the same contents but different pubkeys
            config.bid_observer.block_submitted(
                block.sealed_block,
                normal_signed_submission.submission,
                block.trace,
                builder_name,
                bid_metadata.value.top_competitor_bid.unwrap_or_default(),
            );
        })
    }
}

pub async fn run_submit_to_relays_job_and_metrics(
    pending_bid: Arc<PendingBlockCell>,
    slot_data: MevBoostSlotData,
    relays: Vec<MevBoostRelayBidSubmitter>,
    config: Arc<SubmissionConfig>,
    cancel: CancellationToken,
    competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
) {
    let last_build_block_info = run_submit_to_relays_job(
        pending_bid,
        slot_data,
        relays,
        config,
        cancel,
        competition_bid_value_source,
    )
    .await;
    if let Some(last_build_block_info) = last_build_block_info {
        if last_build_block_info.bid_value > last_build_block_info.true_bid_value {
            inc_subsidized_blocks(false);
            add_subsidy_value(
                last_build_block_info.bid_value - last_build_block_info.true_bid_value,
                false,
            );
        }
    }
}

async fn submit_bid_to_the_relay(
    relay: &MevBoostRelayBidSubmitter,
    cancel: CancellationToken,
    signed_submit_request: SubmitBlockRequestWithMetadata,
    optimistic: bool,
) {
    let submit_start = Instant::now();

    if !relay.can_submit_bid() {
        trace!("Relay submission is skipped due to rate limit");
        return;
    }

    let relay_result = tokio::select! {
        _ = cancel.cancelled() => {
            return;
        },
        res = relay.submit_block(&signed_submit_request) => res
    };
    let submit_time = submit_start.elapsed();
    match relay_result {
        Ok(()) => {
            trace!("Block submitted to the relay successfully");
            add_relay_submit_time(relay.id(), submit_time);
            inc_relay_accepted_submissions(relay.id(), optimistic);
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
                &signed_submit_request.submission,
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
    }
}

/// Real life BuilderSinkFactory that send the blocks to the Relay
#[derive(Debug)]
pub struct RelaySubmitSinkFactory {
    submission_config: Arc<SubmissionConfig>,
    /// Real relays (!MevBoostRelayBidSubmitter::test_relay())
    /// We submit to these only if the MevBoostRelayID is included on the MevBoostSlotData of the slot.
    relays: HashMap<MevBoostRelayID, MevBoostRelayBidSubmitter>,
    /// Test relays (MevBoostRelayBidSubmitter::test_relay())
    /// Always included on submissions.
    test_relays: Vec<MevBoostRelayBidSubmitter>,
}

impl RelaySubmitSinkFactory {
    pub fn new(
        submission_config: SubmissionConfig,
        relays: Vec<MevBoostRelayBidSubmitter>,
    ) -> Self {
        let test_relays = relays.iter().filter(|r| r.test_relay()).cloned().collect();
        let relays = relays
            .into_iter()
            .filter(|r| !r.test_relay())
            .map(|relay| (relay.id().clone(), relay))
            .collect();
        Self {
            submission_config: Arc::new(submission_config),
            relays,
            test_relays,
        }
    }
}

impl BuilderSinkFactory for RelaySubmitSinkFactory {
    fn create_builder_sink(
        &self,
        slot_data: MevBoostSlotData,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        cancel: CancellationToken,
    ) -> Box<dyn BlockBuildingSink> {
        let pending_block_cell = Arc::new(PendingBlockCell::default());

        let relays = slot_data
            .relays
            .iter()
            .flat_map(|id| self.relays.get(id))
            .chain(self.test_relays.iter())
            .cloned()
            .collect();
        tokio::spawn(run_submit_to_relays_job_and_metrics(
            pending_block_cell.clone(),
            slot_data,
            relays,
            self.submission_config.clone(),
            cancel,
            competition_bid_value_source,
        ));
        Box::new(PendingBlockCellToBlockBuildingSink { pending_block_cell })
    }
}
