use ahash::HashMap;
/// Unfinished block processing handles blocks that are produced by block building algorithms.
///
/// 1. Block building algorithm produces unfinished blocks `BiddableUnfinishedBlock` and submits it to the `UnfinishedBuiltBlocksInput`
/// 2. Block cache is updated from the last unfinished block. Its used to share data about built blocks between different algorithms.
/// 3. Then we select next block to use for submission from the blocks built by different algorithms (`BestBlockFromAlgorithms`)
/// 4. Then this block is finalized (`prefinalize_worker` thread)
/// 5. We notify bidding service about new block.
/// 6. Bidding service asks to finalize that block with concrete proposer value  
/// 7. Finalized block is adjusted to pay chosen amount to the proposer (`finalize_worker` thread)
/// 8. Resulting block is submitted to `BlockBuildingSink` (in running builder its used by a thread that submits block to relays).
///
/// Alternatively if configured (adjust_finalized_blocks = true) to run using old flow `prefinalize_worker` would not do anything with the block
/// and `finalize_worker` would do full finalization instead of adjustment of the finalize block.
use alloy_primitives::{utils::format_ether, I256, U256};
use derivative::Derivative;
use parking_lot::Mutex;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;

use tracing::{error, info, trace, warn};

use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        builders::{
            block_building_helper::{
                BiddableUnfinishedBlock, BlockBuildingHelper, BlockBuildingHelperError,
                FinalizeBlockResult,
            },
            BuiltBlockId,
        },
        InsertPayoutTxErr, ThreadBlockBuildingContext,
    },
    live_builder::{
        block_output::{
            bidding_service_interface::RelaySet, relay_submit::MultiRelayBlockBuildingSink,
        },
        payload_events::MevBoostSlotData,
        wallet_balance_watcher::WalletBalanceWatcher,
    },
    provider::StateProviderFactory,
    telemetry::{add_block_multi_bid_copy_duration, add_trigger_to_bid_round_trip_time},
    utils::sync::Watch,
};

use super::{
    best_block_from_algorithms::BestBlockFromAlgorithms,
    bidding_service_interface::{
        BiddingService, BlockSealInterfaceForSlotBidder, BuiltBlockDescriptorForSlotBidder,
        SlotBidder, SlotBidderSealBidCommand,
    },
    relay_submit::RelaySubmitSinkFactory,
};

use crate::live_builder::building::built_block_cache::BuiltBlockCache;

/// UnfinishedBlockBuildingSinkFactory creates UnfinishedBuiltBlocksInput
/// and related workers for each slot
/// For each slot it creates:
/// 1. UnfinishedBuiltBlocksInput and starts `prefinalize_worker` and `finalize_worker` threads.
/// 2. SlotBidder from BiddingService to manage bidding values for the sealed blocks
/// 3. BlockBuildingSink to send finished blocks for relay submission
#[derive(Derivative)]
#[derivative(Debug)]
pub struct UnfinishedBuiltBlocksInputFactory<P> {
    /// Factory for the SlotBidder for blocks.
    #[derivative(Debug = "ignore")]
    bidding_service: Arc<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: RelaySubmitSinkFactory,
    wallet_balance_watcher: WalletBalanceWatcher<P>,
    /// If set to true blocks will be finalized before notifying BiddingService
    /// This reduces latency for creating block with concrete proposer payout value.
    adjust_finalized_blocks: bool,
    /// relay sets well get on bids.
    relay_sets: Vec<RelaySet>,
}

impl<P: StateProviderFactory> UnfinishedBuiltBlocksInputFactory<P> {
    pub fn new(
        bidding_service: Arc<dyn BiddingService>,
        block_sink_factory: RelaySubmitSinkFactory,
        wallet_balance_watcher: WalletBalanceWatcher<P>,
        adjust_finalized_blocks: bool,
        relay_sets: Vec<RelaySet>,
    ) -> Self {
        Self {
            bidding_service,
            block_sink_factory,
            wallet_balance_watcher,
            adjust_finalized_blocks,
            relay_sets,
        }
    }

    pub fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        built_block_cache: Arc<BuiltBlockCache>,
        cancel: CancellationToken,
    ) -> UnfinishedBuiltBlocksInput {
        match self
            .wallet_balance_watcher
            .update_to_block(slot_data.block() - 1)
        {
            Ok(landed_blocks) => self
                .bidding_service
                .update_new_landed_blocks_detected(&landed_blocks),
            Err(err) => {
                error!(?err, "Error updating wallet state");
                self.bidding_service
                    .update_failed_reading_new_landed_blocks()
            }
        }

        let input = UnfinishedBuiltBlocksInput::new(
            built_block_cache,
            slot_data.relay_registrations.keys().cloned().collect(),
            self.adjust_finalized_blocks,
            self.relay_sets.clone(),
            cancel.clone(),
        );

        let slot_bidder = self.bidding_service.create_slot_bidder(
            slot_data.slot_block_id(),
            slot_data.timestamp(),
            Box::new(input.clone()),
            cancel.clone(),
        );

        let input_clone = input.clone();
        std::thread::Builder::new()
            .name("prefinalize_worker".into())
            .spawn(move || input_clone.run_prefinalize_thread(slot_bidder))
            .unwrap();

        let block_sink: Arc<dyn MultiRelayBlockBuildingSink> = self
            .block_sink_factory
            .create_builder_sink(slot_data.clone(), cancel.clone())
            .into();

        for (relay_set, last_finalize_command) in input.last_finalize_commands.iter() {
            let finalized_blocks = input.pre_finalized_multi_blocks.clone();
            let cancellation_token = cancel.clone();
            let adjust_finalized_blocks = self.adjust_finalized_blocks;
            let relay_set = relay_set.clone();
            let last_finalize_command = last_finalize_command.clone();
            let block_sink = block_sink.clone();
            std::thread::Builder::new()
                .name("finalize_worker".into())
                .spawn(move || {
                    UnfinishedBuiltBlocksInput::run_finalize_thread(
                        relay_set,
                        block_sink,
                        finalized_blocks,
                        last_finalize_command,
                        adjust_finalized_blocks,
                        cancellation_token,
                    )
                })
                .unwrap();
        }
        input
    }
}

/// Prefinalized blocks must carry ThreadBlockBuildingContext with them because
/// it contains cached state that would be used in adjust_finalized_block
#[derive(Derivative)]
#[derivative(Debug)]
struct PrefinalizedBlockInner {
    #[derivative(Debug = "ignore")]
    block_building_helper: Box<dyn BlockBuildingHelper>,
    local_ctx: Option<ThreadBlockBuildingContext>,
}

impl PrefinalizedBlockInner {
    fn finalize_block(
        &mut self,
        value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
        adjust_finalized_blocks: bool,
    ) -> Result<Option<FinalizeBlockResult>, BlockBuildingHelperError> {
        if let Some(local_ctx) = self.local_ctx.as_mut() {
            if adjust_finalized_blocks {
                self.block_building_helper
                    .adjust_finalized_block(local_ctx, value, subsidy, seen_competition_bid)
                    .map(Some)
            } else {
                // we clone here because finalizing block multiple times is not supported
                self.block_building_helper
                    .box_clone()
                    .finalize_block(local_ctx, value, subsidy, seen_competition_bid)
                    .map(Some)
            }
        } else {
            Ok(None)
        }
    }
}

/// Prefinalized block ready to be finalized for a specific relay set whose finalizing thread is listening to finalize_input.
#[derive(Debug, Clone)]
struct PrefinalizedBlock {
    block_id: BuiltBlockId,
    inner: Arc<Mutex<PrefinalizedBlockInner>>,
    pub sent_to_bidder: OffsetDateTime,
    pub chosen_as_best_at: OffsetDateTime,
}

impl PrefinalizedBlock {
    fn new(
        block_id: BuiltBlockId,
        chosen_as_best_at: OffsetDateTime,
        sent_to_bidder: OffsetDateTime,
        block_building_helper: Box<dyn BlockBuildingHelper>,
        local_ctx: ThreadBlockBuildingContext,
    ) -> Self {
        Self {
            block_id,
            inner: Arc::new(Mutex::new(PrefinalizedBlockInner {
                block_building_helper,
                local_ctx: Some(local_ctx),
            })),
            sent_to_bidder,
            chosen_as_best_at,
        }
    }
}

#[derive(Debug)]
struct FinalizeCommand {
    prefinalized_block: PrefinalizedBlock,
    value: U256,
    subsidy: I256,
    seen_competition_bid: Option<U256>,
    /// Bid received from the bidder (UnfinishedBuiltBlocksInput::seal_command)
    bid_received_at: OffsetDateTime,
    /// Bid sent to the sealer thread
    sent_to_sealer: OffsetDateTime,
    /// Overhead added by creating the MultiPrefinalizedBlock which makes some extra copies.
    multi_bid_copy_duration: Duration,
}

#[derive(Debug, Clone)]
struct PrefinalizedBlockWithFinalizeInput {
    pub prefinalized_block: PrefinalizedBlock,
    pub finalize_input: Arc<Watch<FinalizeCommand>>,
}

/// PrefinalizedBlock that we should use for each relay set since the each have their one finalize thread and data.
#[derive(Debug, Clone)]
struct MultiPrefinalizedBlock {
    pub block_id: BuiltBlockId,
    pub prefinalized_blocks_by_relay_set: HashMap<RelaySet, PrefinalizedBlockWithFinalizeInput>,
    pub creation_duration: Duration,
}

impl MultiPrefinalizedBlock {
    /// Creates one PrefinalizedBlock per RelaySet cloning block_building_helper/local_ctx for all but the last one.
    fn new(
        block_id: BuiltBlockId,
        last_finalize_commands: &HashMap<RelaySet, Arc<Watch<FinalizeCommand>>>,
        chosen_as_best_at: OffsetDateTime,
        sent_to_bidder: OffsetDateTime,
        block_building_helper: Box<dyn BlockBuildingHelper>,
        local_ctx: ThreadBlockBuildingContext,
    ) -> Self {
        let start = Instant::now();
        let last_index = last_finalize_commands.len() - 1;
        let mut prefinalized_blocks_by_relay_set = HashMap::default();

        let mut insert_prefinalized_block =
            |block_building_helper, local_ctx, relay_set, finalize_input| {
                let prefinalized_block = PrefinalizedBlock::new(
                    block_id,
                    chosen_as_best_at,
                    sent_to_bidder,
                    block_building_helper,
                    local_ctx,
                );
                prefinalized_blocks_by_relay_set.insert(
                    relay_set,
                    PrefinalizedBlockWithFinalizeInput {
                        prefinalized_block,
                        finalize_input,
                    },
                );
            };
        for (index, (relay_set, last_finalize_command)) in last_finalize_commands.iter().enumerate()
        {
            if index != last_index {
                insert_prefinalized_block(
                    block_building_helper.box_clone(),
                    local_ctx.clone(),
                    relay_set.clone(),
                    last_finalize_command.clone(),
                );
            } else {
                insert_prefinalized_block(
                    block_building_helper,
                    local_ctx,
                    relay_set.clone(),
                    last_finalize_command.clone(),
                );
                break;
            }
        }

        let creation_duration = start.elapsed();
        add_block_multi_bid_copy_duration(creation_duration);
        Self {
            block_id,
            prefinalized_blocks_by_relay_set,
            creation_duration,
        }
    }
}

/// UnfinishedBuiltBlocksInput is the main struct that handles the unfinished blocks.
/// A run_finalize_thread is spawned for each relay set.
/// Has 2 modes:
/// - adjust_finalized_blocks: we should end with only using this mode.
///   New blocks are stored on last_unfinalized_block
///   run_prefinalize_thread polls last_unfinalized_block prefinalizes them (calling block_building_helper.finalize_block) and adds them to finalized_blocks
///   When we get a bid (seal_command) we search for the corresponding prefinalized block in finalized_blocks and set a new last_finalize_command for the associated relay set.
///   run_finalize_thread polls last_finalize_command and finalizes them by adjusting the payout value (command.finalize_block calls block_building_helper.adjust_finalized_block)
///   block is send! self.block_building_sink.new_block
///
/// - !adjust_finalized_blocks: to be deprecated.
///   New blocks are stored on last_unfinalized_block
///   run_prefinalize_thread polls last_unfinalized_block adds them to finalized_blocks (no work is done here)
///   When we get a bid (seal_command) we search for the corresponding prefinalized block in finalized_blocks and set a new last_finalize_command for the associated relay set.
///   run_finalize_thread polls last_finalize_command and seals if from scratch (command.finalize_block calls block_building_helper.finalize_block)
///   block is send! self.block_building_sink.new_block
#[derive(Derivative, Clone)]
#[derivative(Debug)]
pub struct UnfinishedBuiltBlocksInput {
    /// We call update_from_new_unfinished_block for each new_block.
    built_block_cache: Arc<BuiltBlockCache>,
    best_block_from_algorithms: Arc<Mutex<BestBlockFromAlgorithms>>,

    /// Last unfinalized block we got on new_block.
    /// It's waiting to be prefinalized by run_prefinalize_thread.
    #[derivative(Debug = "ignore")]
    last_unfinalized_block: Arc<Watch<BiddableUnfinishedBlock>>,

    /// We keep old PrefinalizedBlockInner to recycle the ThreadBlockBuildingContext for new blocks.
    unused_prefinalized_block_inners: Arc<Mutex<Vec<Arc<Mutex<PrefinalizedBlockInner>>>>>,
    last_block_id: Arc<Mutex<u64>>,
    /// run_prefinalize_thread leaves blocks here (can be prefinalized or not depending on adjust_finalized_blocks)
    pre_finalized_multi_blocks: Arc<Mutex<Vec<MultiPrefinalizedBlock>>>,

    /// Set by seal_command.
    /// There is one spawned run_finalize_thread polling the asociated Watch<FinalizeCommand>.
    /// Each run_finalize_thread finalizes the FinalizeCommands (bid adjust or full seal depending on adjust_finalized_blocks) and sends them to block_building_sink.
    last_finalize_commands: HashMap<RelaySet, Arc<Watch<FinalizeCommand>>>,

    cancellation_token: CancellationToken,
    /// See [UnfinishedBuiltBlocksInput] comments.
    adjust_finalized_blocks: bool,
    /// Registered relays for this slot, useful to avoid sealing bids for relays that are not registered for this slot.
    registered_relays: Vec<MevBoostRelayID>,
}

impl UnfinishedBuiltBlocksInput {
    fn new(
        built_block_cache: Arc<BuiltBlockCache>,
        registered_relays: Vec<MevBoostRelayID>,
        adjust_finalized_blocks: bool,
        relay_sets: Vec<RelaySet>,
        cancellation_token: CancellationToken,
    ) -> Self {
        let last_finalize_commands = relay_sets
            .iter()
            .map(|relay_set| (relay_set.clone(), Arc::new(Watch::new())))
            .collect();
        Self {
            built_block_cache,
            best_block_from_algorithms: Arc::new(Mutex::new(BestBlockFromAlgorithms::default())),
            last_unfinalized_block: Arc::new(Watch::new()),
            unused_prefinalized_block_inners: Arc::new(Mutex::new(Vec::new())),
            last_block_id: Arc::new(Mutex::new(0)),
            pre_finalized_multi_blocks: Arc::new(Mutex::new(Vec::new())),
            last_finalize_commands,
            cancellation_token,
            adjust_finalized_blocks,
            registered_relays,
        }
    }

    pub fn new_block(&self, block: BiddableUnfinishedBlock) {
        self.built_block_cache
            .update_from_new_unfinished_block(block.block());

        let mut block = if let Some(block) = self
            .best_block_from_algorithms
            .lock()
            .update_with_new_block(block)
        {
            block
        } else {
            return;
        };
        block.chosen_as_best_at = OffsetDateTime::now_utc();
        info!(block_id=block.id().0,true_block_value = ?block.true_block_value,chosen_as_best_at=?block.chosen_as_best_at,algo=block.block.builder_name(), "New best block chosen");

        let log_span = create_logging_span(block.block());
        let _guard = log_span.enter();

        trace!("New unfinalized block");

        // update last_unfinalized_block
        self.last_unfinalized_block.set(block);
    }

    fn seal_command(&self, bid: SlotBidderSealBidCommand) {
        if let Some(trigger_creation_time) = bid.trigger_creation_time {
            let now = time::OffsetDateTime::now_utc();
            let roundtrip = now - trigger_creation_time;
            add_trigger_to_bid_round_trip_time(roundtrip);
        }
        self.do_seal_command(bid);
    }

    fn do_seal_command(&self, bid: SlotBidderSealBidCommand) {
        let bid_received_at = OffsetDateTime::now_utc();
        let id_span = tracing::info_span!("block_id", block_id = bid.block_id.0);
        let _guard_id_span = id_span.enter();

        trace!(?bid, "Received seal command");

        let mut unused_multi_blocks = Vec::new();
        let mut found_multi_block: Option<MultiPrefinalizedBlock> = None;
        {
            let mut pre_finalized_blocks = self.pre_finalized_multi_blocks.lock();
            let mut i = 0;
            while i < pre_finalized_blocks.len() {
                if pre_finalized_blocks[i].block_id.0 < bid.block_id.0 {
                    unused_multi_blocks.push(pre_finalized_blocks.remove(i));
                    continue;
                }
                if pre_finalized_blocks[i].block_id == bid.block_id {
                    found_multi_block = Some(pre_finalized_blocks[i].clone());
                    break;
                }
                i += 1;
            }
        }

        {
            let mut unused_prefinalized_block_inners = self.unused_prefinalized_block_inners.lock();
            for unused_block in unused_multi_blocks {
                if let Some(prefinalized_block_with_finalize_input) = unused_block
                    .prefinalized_blocks_by_relay_set
                    .values()
                    .next()
                {
                    unused_prefinalized_block_inners.push(
                        prefinalized_block_with_finalize_input
                            .prefinalized_block
                            .inner
                            .clone(),
                    );
                }
            }
        }

        if let Some(mut multi_prefinalized_block) = found_multi_block {
            for payout_info in bid.payout_info.into_iter() {
                if let Some(prefinalized_block_with_finalize_input) = multi_prefinalized_block
                    .prefinalized_blocks_by_relay_set
                    .remove(&payout_info.relays)
                {
                    let sent_to_sealer = OffsetDateTime::now_utc();
                    let finalize_command = FinalizeCommand {
                        prefinalized_block: prefinalized_block_with_finalize_input
                            .prefinalized_block,
                        value: payout_info.payout_tx_value,
                        seen_competition_bid: bid.seen_competition_bid,
                        bid_received_at,
                        sent_to_sealer,
                        subsidy: payout_info.subsidy,
                        multi_bid_copy_duration: multi_prefinalized_block.creation_duration,
                    };
                    prefinalized_block_with_finalize_input
                        .finalize_input
                        .set(finalize_command);
                } else {
                    error!(
                        "Seal command discarded, last_finalize_command was not found for relay set"
                    );
                }
            }
        } else {
            warn!("Seal command discarded, prefinalized block was not found");
        }
    }
}

// prefinalize_worker
impl UnfinishedBuiltBlocksInput {
    fn local_ctx(&self) -> ThreadBlockBuildingContext {
        // we try to reuse ThreadBlockBuildingContext from previously built blocks (as they contain useful caches)
        if let Some(last_prefin_block) = self.unused_prefinalized_block_inners.lock().pop() {
            let mut inner = last_prefin_block.lock();
            inner.local_ctx.take().unwrap_or_default()
        } else {
            ThreadBlockBuildingContext::default()
        }
    }

    fn run_prefinalize_thread(self, slot_bidder: Arc<dyn SlotBidder>) {
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }
            let next_block = if let Some(block) = self.last_unfinalized_block.wait_for_data() {
                block
            } else {
                continue;
            };

            let log_span = create_logging_span(next_block.block());
            let _guard = log_span.enter();

            let block_id = next_block.block.built_block_trace().build_block_id;
            let id_span = tracing::info_span!("block_id", block_id = block_id.0);
            let _guard_id_span = id_span.enter();
            let mut block_descriptor =
                BuiltBlockDescriptorForSlotBidder::new(block_id, &next_block);
            let mut local_ctx = self.local_ctx();
            let chosen_as_best_at = next_block.chosen_as_best_at;
            let mut block_building_helper = next_block.into_building_helper();
            if self.adjust_finalized_blocks {
                let value = match block_building_helper.true_block_value() {
                    Ok(value) => value,
                    Err(BlockBuildingHelperError::InsertPayoutTxErr(
                        InsertPayoutTxErr::ProfitTooLow,
                    )) => {
                        trace!("Block profit is too low");
                        continue;
                    }
                    Err(err) => {
                        error!(?err, "Failed to get block true value");
                        continue;
                    }
                };
                match block_building_helper.finalize_block(&mut local_ctx, value, I256::ZERO, None)
                {
                    Ok(_) => {
                        trace!("Prefinalized block");
                    }
                    Err(err) => {
                        if err.is_critical() {
                            error!(?err, "Failed to prefinalize block");
                        }
                        continue;
                    }
                };
            }

            //let multi_prefinalized_block = MultiPrefinalizedBlock::new_single_prefinalized_block(
            let multi_prefinalized_block = MultiPrefinalizedBlock::new(
                block_id,
                &self.last_finalize_commands,
                chosen_as_best_at,
                OffsetDateTime::now_utc(),
                block_building_helper,
                local_ctx,
            );
            self.pre_finalized_multi_blocks
                .lock()
                .push(multi_prefinalized_block);

            // Must update creation time here because since constructor we did some stuff and we want to measure only bidding core timings.
            block_descriptor.creation_time = OffsetDateTime::now_utc();
            slot_bidder.notify_new_built_block(block_descriptor);
            trace!("Notified bidding service");
        }
        trace!("Finished prefinalize_worker");
    }
}

// finalize_worker
impl UnfinishedBuiltBlocksInput {
    fn run_finalize_thread(
        relay_set: RelaySet,
        block_building_sink: Arc<dyn MultiRelayBlockBuildingSink>,
        pre_finalized_blocks: Arc<Mutex<Vec<MultiPrefinalizedBlock>>>,
        last_finalize_command: Arc<Watch<FinalizeCommand>>,
        adjust_finalized_blocks: bool,
        cancellation_token: CancellationToken,
    ) {
        loop {
            if cancellation_token.is_cancelled() {
                break;
            }
            let finalize_command = if let Some(command) = last_finalize_command.wait_for_data() {
                command
            } else {
                continue;
            };
            let picked_by_sealer_at = OffsetDateTime::now_utc();
            let mut command = finalize_command.prefinalized_block.inner.lock();

            let id_span = tracing::info_span!(
                "block_id",
                block_id = finalize_command.prefinalized_block.block_id.0
            );
            let _guard_id_span = id_span.enter();

            let log_span = create_logging_span(command.block_building_helper.as_ref());
            let _guard = log_span.enter();

            let mut result = match command.finalize_block(
                finalize_command.value,
                finalize_command.subsidy,
                finalize_command.seen_competition_bid,
                adjust_finalized_blocks,
            ) {
                Ok(Some(result)) => {
                    trace!("Finalized block");
                    result
                }
                Ok(None) => {
                    warn!("Prefinalized block was discarded");
                    continue;
                }
                Err(err) => {
                    // remove this block from a list of prefinalized blocks as it can be inconsistent
                    pre_finalized_blocks.lock().retain(|block| {
                        block.block_id != finalize_command.prefinalized_block.block_id
                    });

                    let log_error = if adjust_finalized_blocks {
                        // always log this error as its not expected when adjusting blocks
                        true
                    } else {
                        // same as for old flow with finalization, log only critical errors
                        err.is_critical()
                    };

                    if log_error {
                        // when adjusting blocks finalization adjustment should not fail
                        error!(?err, "Failed to finalize prefinalized block");
                    }
                    continue;
                }
            };
            result.block.trace.bid_received_at = finalize_command.bid_received_at;
            result.block.trace.multi_bid_copy_duration = finalize_command.multi_bid_copy_duration;
            result.block.trace.sent_to_sealer = finalize_command.sent_to_sealer;
            result.block.trace.picked_by_sealer_at = picked_by_sealer_at;
            result.block.trace.chosen_as_best_at =
                finalize_command.prefinalized_block.chosen_as_best_at;
            result.block.trace.sent_to_bidder = finalize_command.prefinalized_block.sent_to_bidder;
            block_building_sink.new_block(relay_set.clone(), result.block);
        }
    }
}

impl BlockSealInterfaceForSlotBidder for UnfinishedBuiltBlocksInput {
    fn seal_bid(&self, bid: SlotBidderSealBidCommand) {
        self.seal_command(bid)
    }
}

fn create_logging_span(block_helper: &dyn BlockBuildingHelper) -> tracing::Span {
    let ctx = block_helper.building_context();
    let block = ctx.block();
    let payload_id = ctx.payload_id;
    let builder_name = block_helper.builder_name();
    let true_block_value = format_ether(block_helper.true_block_value().unwrap_or_default());

    tracing::info_span!(
        "unfinished_block",
        block,
        payload_id,
        builder_name,
        true_block_value
    )
}
