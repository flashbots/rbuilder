use crate::{
    building::builders::{BestBlockCell, BuilderSinkFactory},
    flashbots::BlocksProcessorClient,
    live_builder::{bidding::SlotBidder, payload_events::MevBoostSlotData},
    mev_boost::{
        sign_block_for_relay, BLSBlockSigner, RelayError, SubmitBlockErr, SubmitBlockRequest,
    },
    primitives::mev_boost::{MevBoostRelay, MevBoostRelayID},
    telemetry::{
        add_relay_submit_time, add_subsidy_value, inc_blocks_api_errors, inc_conn_relay_errors,
        inc_failed_block_simulations, inc_initiated_submissions, inc_other_relay_errors,
        inc_relay_accepted_submissions, inc_subsidized_blocks, inc_too_many_req_relay_errors,
        measure_block_e2e_latency,
    },
    utils::error_storage::store_error_event,
    validation_api_client::{ValdationError, ValidationAPIClient},
};
use ahash::HashMap;
use alloy_primitives::{utils::format_ether, U256};
use reth::primitives::{ChainSpec, SealedBlock};
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info_span, trace, warn, Instrument};

const SIM_ERROR_CATEGORY: &str = "submit_block_simulation";
const VALIDATION_ERROR_CATEGORY: &str = "validate_block_simulation";

#[derive(Debug, Clone)]
pub struct SubmissionConfig {
    pub chain_spec: Arc<ChainSpec>,
    pub signer: BLSBlockSigner,

    pub dry_run: bool,
    pub validation_api: ValidationAPIClient,

    pub optimistic_enabled: bool,
    pub optimistic_signer: BLSBlockSigner,
    pub optimistic_max_bid_value: U256,
    pub optimistic_prevalidate_optimistic_blocks: bool,

    pub blocks_processor: Option<BlocksProcessorClient>,
    /// Delta relative to slot_time at which we start to submit blocks. Usually negative since we need to start submitting BEFORE the slot time.
    pub slot_delta_to_start_submits: time::Duration,
}

/// run_submit_to_relays_job waits at least MIN_TIME_BETWEEN_BLOCK_CHECK between new block polls to avoid 100% CPU
const MIN_TIME_BETWEEN_BLOCK_CHECK: Duration = Duration::from_millis(5);

/// Values from [`BuiltBlockTrace`]
struct BuiltBlockInfo {
    pub bid_value: U256,
    pub true_bid_value: U256,
}
/// `run_submit_to_relays_job` is a main function for submitting blocks to relays
/// Every 50ms It will take a new best block produced by builders and submit it.
///
/// How submission works:
/// 0. We divide relays into optimistic and non-optimistic (defined in config file)
/// 1. If we are in dry run mode we validate the payload and skip submission to the relays
/// 2. We schedule submissions with non-optimistic key for all non-optimistic relays.
/// 3.1 If "optimistic_enabled" is false or bid_value >= "optimistic_max_bid_value" we schedule submissions with non-optimistic key
/// 3.2 If "optimistic_prevalidate_optimistic_blocks" is false we schedule submissions with optimistic key
/// 3.3 If "optimistic_prevalidate_optimistic_blocks" is true we validate block using validation API and then schedule submissions with optimistic key
/// returns the best bid made
#[allow(clippy::too_many_arguments)]
async fn run_submit_to_relays_job(
    best_bid: BestBlockCell,
    slot_data: MevBoostSlotData,
    relays: Vec<MevBoostRelay>,
    config: SubmissionConfig,
    cancel: CancellationToken,
    slot_bidder: Arc<dyn SlotBidder>,
) -> Option<BuiltBlockInfo> {
    let mut res = None;
    // first, sleep to slot time - slot_delta_to_start_submits
    {
        let submit_start_time = slot_data.timestamp() + config.slot_delta_to_start_submits;
        let sleep_duration = submit_start_time - time::OffsetDateTime::now_utc();
        if sleep_duration.is_positive() {
            sleep(sleep_duration.try_into().unwrap()).await;
        }
    }

    let (normal_relays, optimistic_relays) = {
        let mut normal_relays = Vec::new();
        let mut optimistic_relays = Vec::new();
        for relay in relays {
            if relay.optimistic {
                optimistic_relays.push(relay);
            } else {
                normal_relays.push(relay);
            }
        }
        (normal_relays, optimistic_relays)
    };

    let mut last_bid_value = U256::from(0);
    let mut last_submit_time = Instant::now();
    'submit: loop {
        if cancel.is_cancelled() {
            break 'submit res;
        }

        let time_since_submit = last_submit_time.elapsed();
        if time_since_submit < MIN_TIME_BETWEEN_BLOCK_CHECK {
            sleep(MIN_TIME_BETWEEN_BLOCK_CHECK - time_since_submit).await;
        }
        last_submit_time = Instant::now();

        let block = if let Some(new_block) = best_bid.take_best_block() {
            if new_block.trace.bid_value > last_bid_value {
                last_bid_value = new_block.trace.bid_value;
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
        let submission_optimistic =
            config.optimistic_enabled && block.trace.bid_value < config.optimistic_max_bid_value;
        let best_bid_value = slot_bidder.best_bid_value().unwrap_or_default();
        let submission_span = info_span!(
            "bid",
            bid_value = format_ether(block.trace.bid_value),
            best_bid_value = format_ether(best_bid_value),
            true_bid_value = format_ether(block.trace.true_bid_value),
            block = block.sealed_block.number,
            hash = ?block.sealed_block.header.hash(),
            gas = block.sealed_block.gas_used,
            txs = block.sealed_block.body.len(),
            bundles,
            buidler_name = block.builder_name,
            fill_time_ms = block.trace.fill_time.as_millis(),
            finalize_time_ms = block.trace.finalize_time.as_millis(),
        );
        debug!(
            parent: &submission_span,
            "Submitting bid",
        );
        inc_initiated_submissions(submission_optimistic);

        let (normal_signed_submission, optimistic_signed_submission) = {
            let normal_signed_submission = match sign_block_for_relay(
                &config.signer,
                &block.sealed_block,
                &block.txs_blobs_sidecars,
                &config.chain_spec,
                &slot_data.payload_attributes_event.data,
                slot_data.slot_data.pubkey,
                block.trace.bid_value,
            ) {
                Ok(res) => res,
                Err(err) => {
                    error!(parent: &submission_span, err = ?err, "Error signing block for relay");
                    continue 'submit;
                }
            };
            let optimistic_signed_submission = match sign_block_for_relay(
                &config.optimistic_signer,
                &block.sealed_block,
                &block.txs_blobs_sidecars,
                &config.chain_spec,
                &slot_data.payload_attributes_event.data,
                slot_data.slot_data.pubkey,
                block.trace.bid_value,
            ) {
                Ok(res) => res,
                Err(err) => {
                    error!(parent: &submission_span, err = ?err, "Error signing block for relay");
                    continue 'submit;
                }
            };
            (normal_signed_submission, optimistic_signed_submission)
        };

        if config.dry_run {
            match validate_block(
                &slot_data,
                &normal_signed_submission,
                block.sealed_block.clone(),
                &config,
                cancel.clone(),
            )
            .await
            {
                Ok(()) => {
                    trace!(parent: &submission_span, "Dry run validation passed");
                }
                Err(ValdationError::UnableToValidate(err)) => {
                    warn!(parent: &submission_span, err, "Failed to validate payload");
                }
                Err(ValdationError::ValidationFailed(err)) => {
                    error!(parent: &submission_span, err = ?err, "Dry run validation failed");
                    inc_failed_block_simulations();
                    store_error_event(
                        VALIDATION_ERROR_CATEGORY,
                        &err.to_string(),
                        &normal_signed_submission,
                    );
                }
            }
            continue 'submit;
        }

        measure_block_e2e_latency(&block.trace.included_orders);

        for relay in &normal_relays {
            let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id, optimistic = false);
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

        if submission_optimistic {
            let can_submit = if config.optimistic_prevalidate_optimistic_blocks {
                let start = Instant::now();
                match validate_block(
                    &slot_data,
                    &optimistic_signed_submission,
                    block.sealed_block.clone(),
                    &config,
                    cancel.clone(),
                )
                .await
                {
                    Ok(()) => {
                        trace!(parent: &submission_span,
                            time_ms = start.elapsed().as_millis(),
                            "Optimistic validation passed"
                        );
                        true
                    }
                    Err(ValdationError::UnableToValidate(err)) => {
                        warn!(parent: &submission_span, err = ?err, "Failed to validate optimistic payload");
                        false
                    }
                    Err(ValdationError::ValidationFailed(err)) => {
                        error!(parent: &submission_span, err = ?err, "Optimistic Payload Validation failed");
                        inc_failed_block_simulations();
                        store_error_event(
                            VALIDATION_ERROR_CATEGORY,
                            &err.to_string(),
                            &optimistic_signed_submission,
                        );
                        false
                    }
                }
            } else {
                true
            };

            if can_submit {
                for relay in &optimistic_relays {
                    let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id, optimistic = true);
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
            }
        } else {
            // non-optimistic submission to optimistic relays
            for relay in &optimistic_relays {
                let span = info_span!(parent: &submission_span, "relay_submit", relay = &relay.id, optimistic = false);
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

        if let Some(blocks_processor) = config.blocks_processor.clone() {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let block_processor_result = tokio::select! {
                    _ = cancel.cancelled() => {
                        return;
                    },
                    // NOTE: we only store normal submission here because they have the same contents but different pubkeys
                    res = blocks_processor.submit_built_block(&block.sealed_block, &normal_signed_submission, &block.trace, builder_name, best_bid_value) => res
                };
                if let Err(err) = block_processor_result {
                    inc_blocks_api_errors();
                    warn!(parent: &submission_span, "Failed to submit block to the blocks api: {}", err);
                }
            });
        }
    }
}

pub async fn run_submit_to_relays_job_and_metrics(
    best_bid: BestBlockCell,
    slot_data: MevBoostSlotData,
    relays: Vec<MevBoostRelay>,
    config: SubmissionConfig,
    cancel: CancellationToken,
    slot_bidder: Arc<dyn SlotBidder>,
) {
    let best_bid = run_submit_to_relays_job(
        best_bid.clone(),
        slot_data,
        relays,
        config,
        cancel,
        slot_bidder,
    )
    .await;
    if let Some(best_bid) = best_bid {
        if best_bid.bid_value > best_bid.true_bid_value {
            inc_subsidized_blocks(false);
            add_subsidy_value(best_bid.bid_value - best_bid.true_bid_value, false);
        }
    }
}

async fn validate_block(
    slot_data: &MevBoostSlotData,
    signed_submit_request: &SubmitBlockRequest,
    block: SealedBlock,
    config: &SubmissionConfig,
    cancellation_token: CancellationToken,
) -> Result<(), ValdationError> {
    let withdrawals_root = block.withdrawals_root.unwrap_or_default();

    config
        .validation_api
        .validate_block(
            signed_submit_request,
            slot_data.suggested_gas_limit,
            withdrawals_root,
            block.parent_beacon_block_root,
            cancellation_token,
        )
        .await?;
    Ok(())
}

async fn submit_bid_to_the_relay(
    relay: &MevBoostRelay,
    cancel: CancellationToken,
    signed_submit_request: SubmitBlockRequest,
    optimistic: bool,
) {
    let submit_start = Instant::now();

    if let Some(limiter) = &relay.submission_rate_limiter {
        if limiter.check().is_err() {
            trace!("Relay submission is skipped due to rate limit");
            return;
        }
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
            add_relay_submit_time(&relay.id, submit_time);
            inc_relay_accepted_submissions(&relay.id, optimistic);
        }
        Err(SubmitBlockErr::PayloadDelivered | SubmitBlockErr::PastSlot) => {
            trace!("Block already delivered by the relay, cancelling");
            cancel.cancel();
        }
        Err(SubmitBlockErr::BidBelowFloor | SubmitBlockErr::PayloadAttributesNotKnown) => {
            trace!(err = ?relay_result.unwrap_err(), "Block not accepted by the relay");
        }
        Err(SubmitBlockErr::SimError(err)) => {
            inc_failed_block_simulations();
            error!(err = ?err, "Error block simulation fail, cancelling");
            store_error_event(SIM_ERROR_CATEGORY, &err.to_string(), &signed_submit_request);
            cancel.cancel();
        }
        Err(SubmitBlockErr::RelayError(RelayError::TooManyRequests)) => {
            trace!("Too many requests error submitting block to the relay");
            inc_too_many_req_relay_errors(&relay.id);
        }
        Err(SubmitBlockErr::RelayError(RelayError::ConnectionError))
        | Err(SubmitBlockErr::RelayError(RelayError::RequestError(_))) => {
            trace!(err = ?relay_result.unwrap_err(), "Connection error submitting block to the relay");
            inc_conn_relay_errors(&relay.id);
        }
        Err(SubmitBlockErr::BlockKnown) => {
            trace!("Block already known");
        }
        Err(SubmitBlockErr::RelayError(err)) => {
            warn!(err = ?err, "Error submitting block to the relay");
            inc_other_relay_errors(&relay.id);
        }
        Err(SubmitBlockErr::RPCConversionError(err)) => {
            error!(
                err = ?err,
                "RPC conversion error (illegal submission?) submitting block to the relay",
            );
        }
        Err(SubmitBlockErr::RPCSerializationError(err)) => {
            error!(
                err = ?err,
                "SubmitBlock serialization error submitting block to the relay",
            );
        }
        Err(SubmitBlockErr::InvalidHeader) => {
            error!("Invalid authorization header submitting block to the relay");
        }
    }
}

/// Real life BuilderSinkFactory that send the blocks to the Relay
pub struct RelaySubmitSinkFactory {
    submission_config: SubmissionConfig,
    relays: HashMap<MevBoostRelayID, MevBoostRelay>,
}

impl RelaySubmitSinkFactory {
    pub fn new(submission_config: SubmissionConfig, relays: Vec<MevBoostRelay>) -> Self {
        let relays = relays
            .into_iter()
            .map(|relay| (relay.id.clone(), relay))
            .collect();
        Self {
            submission_config,
            relays,
        }
    }
}

impl BuilderSinkFactory for RelaySubmitSinkFactory {
    type SinkType = BestBlockCell;

    fn create_builder_sink(
        &self,
        slot_data: MevBoostSlotData,
        slot_bidder: Arc<dyn SlotBidder>,
        cancel: CancellationToken,
    ) -> BestBlockCell {
        let best_bid = BestBlockCell::default();

        let relays = slot_data
            .relays
            .iter()
            .map(|id| {
                self.relays
                    .get(id)
                    .expect("Submission job is missing relay")
                    .clone()
            })
            .collect();
        tokio::spawn(run_submit_to_relays_job_and_metrics(
            best_bid.clone(),
            slot_data,
            relays,
            self.submission_config.clone(),
            cancel,
            slot_bidder,
        ));
        best_bid
    }
}
