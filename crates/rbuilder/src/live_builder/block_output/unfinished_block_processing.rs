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
use std::time::Duration;

use alloy_primitives::{utils::format_ether, U256};
use derivative::Derivative;
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
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
        payload_events::MevBoostSlotData, wallet_balance_watcher::WalletBalanceWatcher,
    },
    provider::StateProviderFactory,
    telemetry::add_trigger_to_bid_round_trip_time,
};

use super::{
    best_block_from_algorithms::BestBlockFromAlgorithms,
    bidding_service_interface::{
        BiddingService, BlockSealInterfaceForSlotBidder, BuiltBlockDescriptorForSlotBidder,
        SlotBidder, SlotBidderSealBidCommand, SlotBlockId,
    },
    relay_submit::RelaySubmitSinkFactory,
};

use super::relay_submit::BlockBuildingSink;
use crate::live_builder::building::built_block_cache::BuiltBlockCache;

const THREAD_BLOCKING_DURATION: Duration = Duration::from_millis(100);

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
}

impl<P: StateProviderFactory> UnfinishedBuiltBlocksInputFactory<P> {
    pub fn new(
        bidding_service: Arc<dyn BiddingService>,
        block_sink_factory: RelaySubmitSinkFactory,
        wallet_balance_watcher: WalletBalanceWatcher<P>,
        adjust_finalized_blocks: bool,
    ) -> Self {
        Self {
            bidding_service,
            block_sink_factory,
            wallet_balance_watcher,
            adjust_finalized_blocks,
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

        let finished_block_sink = self
            .block_sink_factory
            .create_builder_sink(slot_data.clone(), cancel.clone());

        let input = UnfinishedBuiltBlocksInput::new(
            built_block_cache,
            finished_block_sink,
            self.adjust_finalized_blocks,
            cancel.clone(),
        );

        let slot_bidder = self.bidding_service.create_slot_bidder(
            SlotBlockId::new(
                slot_data.slot(),
                slot_data.block(),
                slot_data.parent_block_hash(),
            ),
            slot_data.timestamp(),
            Box::new(input.clone()),
            cancel.clone(),
        );

        let input_clone = input.clone();
        std::thread::Builder::new()
            .name("prefinalize_worker".into())
            .spawn(move || input_clone.run_prefinalize_thread(slot_bidder))
            .unwrap();

        let input_clone = input.clone();
        std::thread::Builder::new()
            .name("finalize_worker".into())
            .spawn(move || input_clone.run_finalize_thread())
            .unwrap();

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
        seen_competition_bid: Option<U256>,
        adjust_finalized_blocks: bool,
    ) -> Result<Option<FinalizeBlockResult>, BlockBuildingHelperError> {
        if let Some(local_ctx) = self.local_ctx.as_mut() {
            if adjust_finalized_blocks {
                self.block_building_helper
                    .adjust_finalized_block(local_ctx, value, seen_competition_bid)
                    .map(Some)
            } else {
                // we clone here because finalizing block multiple times is not supported
                self.block_building_helper
                    .box_clone()
                    .finalize_block(local_ctx, value, seen_competition_bid)
                    .map(Some)
            }
        } else {
            Ok(None)
        }
    }
}

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
        block_building_helper: Box<dyn BlockBuildingHelper>,
        local_ctx: ThreadBlockBuildingContext,
    ) -> Self {
        Self {
            block_id,
            inner: Arc::new(Mutex::new(PrefinalizedBlockInner {
                block_building_helper,
                local_ctx: Some(local_ctx),
            })),
            sent_to_bidder: OffsetDateTime::now_utc(),
            chosen_as_best_at,
        }
    }
}

#[derive(Debug)]
struct FinalizeCommand {
    prefinalized_block: PrefinalizedBlock,
    value: U256,
    seen_competition_bid: Option<U256>,
    /// Bid received from the bidder (UnfinishedBuiltBlocksInput::seal_command)
    bid_received_at: OffsetDateTime,
    /// Bid sent to the sealer thread
    sent_to_sealer: OffsetDateTime,
}

#[derive(Derivative, Clone)]
#[derivative(Debug)]
pub struct UnfinishedBuiltBlocksInput {
    built_block_cache: Arc<BuiltBlockCache>,

    best_block_from_algorithms: Arc<Mutex<BestBlockFromAlgorithms>>,

    #[derivative(Debug = "ignore")]
    last_unfinalized_block: Arc<(Mutex<Option<BiddableUnfinishedBlock>>, Condvar)>,

    unused_prefinalized_blocks: Arc<Mutex<Vec<PrefinalizedBlock>>>,
    last_block_id: Arc<Mutex<u64>>,
    finalized_blocks: Arc<Mutex<Vec<PrefinalizedBlock>>>,

    last_finalize_command: Arc<(Mutex<Option<FinalizeCommand>>, Condvar)>,

    cancellation_token: CancellationToken,
    #[derivative(Debug = "ignore")]
    block_building_sink: Arc<dyn BlockBuildingSink>,
    adjust_finalized_blocks: bool,
}

impl UnfinishedBuiltBlocksInput {
    fn new(
        built_block_cache: Arc<BuiltBlockCache>,
        block_building_sink: Box<dyn BlockBuildingSink>,
        adjust_finalized_blocks: bool,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            built_block_cache,
            best_block_from_algorithms: Arc::new(Mutex::new(BestBlockFromAlgorithms::default())),
            last_unfinalized_block: Arc::new((Mutex::new(None), Condvar::new())),
            unused_prefinalized_blocks: Arc::new(Mutex::new(Vec::new())),
            last_block_id: Arc::new(Mutex::new(0)),
            finalized_blocks: Arc::new(Mutex::new(Vec::new())),
            last_finalize_command: Arc::new((Mutex::new(None), Condvar::new())),
            cancellation_token,
            block_building_sink: block_building_sink.into(),
            adjust_finalized_blocks,
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
        let (lock, cvar) = &*self.last_unfinalized_block;
        let mut guard = lock.lock();
        *guard = Some(block);
        cvar.notify_one();
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

        let mut unused_blocks = Vec::new();
        let mut found_block: Option<PrefinalizedBlock> = None;
        {
            let mut finalized_blocks = self.finalized_blocks.lock();
            let mut i = 0;
            while i < finalized_blocks.len() {
                if finalized_blocks[i].block_id.0 < bid.block_id.0 {
                    unused_blocks.push(finalized_blocks.remove(i));
                    continue;
                }
                if finalized_blocks[i].block_id == bid.block_id {
                    found_block = Some(finalized_blocks[i].clone());
                    break;
                }
                i += 1;
            }
        }
        self.unused_prefinalized_blocks
            .lock()
            .append(&mut unused_blocks);
        if let Some(prefinalized_block) = found_block {
            let sent_to_sealer = OffsetDateTime::now_utc();
            let finalize_command = FinalizeCommand {
                prefinalized_block,
                value: bid.payout_tx_value,
                seen_competition_bid: bid.seen_competition_bid,
                bid_received_at,
                sent_to_sealer,
            };
            let (lock, cvar) = &*self.last_finalize_command;
            let mut guard = lock.lock();
            *guard = Some(finalize_command);
            cvar.notify_one();
        } else {
            warn!("Seal command discarded, prefinalized block was not found");
        }
    }
}

// prefinalize_worker
impl UnfinishedBuiltBlocksInput {
    fn take_last_unfinalized_block(&self) -> Option<BiddableUnfinishedBlock> {
        let (lock, cvar) = &*self.last_unfinalized_block;
        let mut guard = lock.lock();
        while guard.is_none() {
            let timeout_result = cvar.wait_for(&mut guard, THREAD_BLOCKING_DURATION);
            if timeout_result.timed_out() {
                return None;
            }
        }
        guard.take()
    }

    fn local_ctx(&self) -> ThreadBlockBuildingContext {
        // we try to reuse ThreadBlockBuildingContext from previously built blocks (as they contain useful caches)
        if let Some(last_prefin_block) = self.unused_prefinalized_blocks.lock().pop() {
            let mut inner = last_prefin_block.inner.lock();
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
            let next_block = if let Some(block) = self.take_last_unfinalized_block() {
                block
            } else {
                continue;
            };

            let log_span = create_logging_span(next_block.block());
            let _guard = log_span.enter();

            let block_id = next_block.block.built_block_trace().build_block_id;
            let id_span = tracing::info_span!("block_id", block_id = block_id.0);
            let _guard_id_span = id_span.enter();
            let block_descriptor = BuiltBlockDescriptorForSlotBidder::new(block_id, &next_block);

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
                match block_building_helper.finalize_block(&mut local_ctx, value, None) {
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
            let prefinalized_result = PrefinalizedBlock::new(
                block_id,
                chosen_as_best_at,
                block_building_helper,
                local_ctx,
            );
            self.finalized_blocks.lock().push(prefinalized_result);
            slot_bidder.notify_new_built_block(block_descriptor);
            trace!("Notified bidding service");
        }
        trace!("Finished prefinalize_worker");
    }
}

// finalize_worker
impl UnfinishedBuiltBlocksInput {
    fn take_next_finalize_command(&self) -> Option<FinalizeCommand> {
        let (lock, cvar) = &*self.last_finalize_command;
        let mut guard = lock.lock();
        while guard.is_none() {
            let timeout_result = cvar.wait_for(&mut guard, THREAD_BLOCKING_DURATION);
            if timeout_result.timed_out() {
                return None;
            }
        }
        guard.take()
    }

    fn run_finalize_thread(self) {
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }
            let finalize_command = if let Some(command) = self.take_next_finalize_command() {
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
                finalize_command.seen_competition_bid,
                self.adjust_finalized_blocks,
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
                    self.finalized_blocks.lock().retain(|block| {
                        block.block_id != finalize_command.prefinalized_block.block_id
                    });

                    let log_error = if self.adjust_finalized_blocks {
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
            result.block.trace.sent_to_sealer = finalize_command.sent_to_sealer;
            result.block.trace.picked_by_sealer_at = picked_by_sealer_at;
            result.block.trace.chosen_as_best_at =
                finalize_command.prefinalized_block.chosen_as_best_at;
            result.block.trace.sent_to_bidder = finalize_command.prefinalized_block.sent_to_bidder;
            self.block_building_sink.new_block(result.block);
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
