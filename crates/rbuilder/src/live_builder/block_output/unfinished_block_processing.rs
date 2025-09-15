use std::time::Duration;

use alloy_primitives::U256;
use derivative::Derivative;
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;

use tracing::{error, warn};

use ahash::HashMap;
use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        builders::block_building_helper::{
            BiddableUnfinishedBlock, BlockBuildingHelper, BlockBuildingHelperError,
            FinalizeBlockResult,
        },
        ThreadBlockBuildingContext,
    },
    live_builder::{
        payload_events::MevBoostSlotData, wallet_balance_watcher::WalletBalanceWatcher,
    },
    provider::StateProviderFactory,
};

use super::{
    bidding_service_interface::{
        BiddingService, BlockId, BlockSealInterfaceForSlotBidder,
        BuiltBlockDescriptorForSlotBidder, SlotBidder, SlotBidderSealBidCommand, SlotBlockId,
    },
    relay_submit::RelaySubmitSinkFactory,
};

use super::relay_submit::BlockBuildingSink;
use crate::live_builder::building::built_block_cache::BuiltBlockCache;

const THREAD_BLOCKING_DURATION: Duration = Duration::from_millis(100);

/// UnfinishedBlockBuildingSinkFactory to bid blocks against the competition.
/// Blocks are given to a slot bidder (UnfinishedBlockBuildingSink created per block by the BiddingService).
/// Slot bidder bids using a SequentialSealerBidMaker (created per block).
/// SequentialSealerBidMaker sends the bids to a BlockBuildingSink (created per block).
#[derive(Derivative)]
#[derivative(Debug)]
pub struct UnfinishedBuiltBlocksInputFactory<P> {
    /// Factory for the SlotBidder for blocks.
    #[derivative(Debug = "ignore")]
    bidding_service: Arc<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: RelaySubmitSinkFactory,
    wallet_balance_watcher: WalletBalanceWatcher<P>,
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
            .spawn(move || input_clone.run_prefinilze_thread(slot_bidder))
            .unwrap();

        let input_clone = input.clone();
        std::thread::Builder::new()
            .name("finalize_worker".into())
            .spawn(move || input_clone.run_finalize_thread())
            .unwrap();

        input
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
struct PrefinalizedBlockInner {
    #[derivative(Debug = "ignore")]
    block_building_helper: Box<dyn BlockBuildingHelper>,
    local_ctx: Option<ThreadBlockBuildingContext>,
}

impl PrefinalizedBlockInner {
    fn finalize_prefinalized_block(
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
                self.block_building_helper
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
    block_id: BlockId,
    inner: Arc<Mutex<PrefinalizedBlockInner>>,
}

impl PrefinalizedBlock {
    fn new(
        block_id: BlockId,
        block_building_helper: Box<dyn BlockBuildingHelper>,
        local_ctx: ThreadBlockBuildingContext,
    ) -> Self {
        Self {
            block_id,
            inner: Arc::new(Mutex::new(PrefinalizedBlockInner {
                block_building_helper,
                local_ctx: Some(local_ctx),
            })),
        }
    }
}

#[derive(Debug)]
struct FinalizeCommand {
    prefinalized_block: PrefinalizedBlock,
    value: U256,
    seen_competition_bid: Option<U256>,
}

#[derive(Derivative, Default)]
#[derivative(Debug)]
struct BestBlockFromAlgorithms {
    #[derivative(Debug = "ignore")]
    last_block_by_algorithm: HashMap<String, BiddableUnfinishedBlock>,
    last_best_block_hash: u64,
}

impl BestBlockFromAlgorithms {
    fn new_block(
        &mut self,
        unfinished_block: BiddableUnfinishedBlock,
    ) -> Option<BiddableUnfinishedBlock> {
        self.last_block_by_algorithm.insert(
            unfinished_block.block.builder_name().to_string(),
            unfinished_block,
        );
        let last_best_block = self
            .last_block_by_algorithm
            .values()
            .max_by_key(|bb| bb.true_block_value)
            .unwrap();
        let best_block_hash = last_best_block
            .block
            .built_block_trace()
            .transactions_hash();
        if self.last_best_block_hash == best_block_hash {
            None
        } else {
            self.last_best_block_hash = best_block_hash;
            Some(last_best_block.clone())
        }
    }
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
    block_building_sink: Arc<Mutex<Box<dyn BlockBuildingSink>>>,
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
            block_building_sink: Arc::new(Mutex::new(block_building_sink)),
            adjust_finalized_blocks,
        }
    }

    pub fn new_block(&self, block: BiddableUnfinishedBlock) {
        self.built_block_cache
            .update_from_new_unfinished_block(block.block());

        let block = if let Some(block) = self.best_block_from_algorithms.lock().new_block(block) {
            block
        } else {
            return;
        };

        let (lock, cvar) = &*self.last_unfinalized_block;
        let mut guard = lock.lock();
        *guard = Some(block);
        cvar.notify_one();
    }

    fn get_next_block(&self) -> Option<BiddableUnfinishedBlock> {
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

    fn next_block_id(&self) -> BlockId {
        let mut last_id = self.last_block_id.lock();
        let id = BlockId(*last_id);
        *last_id += 1;
        id
    }

    fn local_ctx(&self) -> ThreadBlockBuildingContext {
        if let Some(last_prefin_block) = self.unused_prefinalized_blocks.lock().pop() {
            let mut inner = last_prefin_block.inner.lock();
            inner.local_ctx.take().unwrap_or_default()
        } else {
            ThreadBlockBuildingContext::default()
        }
    }

    fn run_prefinilze_thread(self, slot_bidder: Arc<dyn SlotBidder>) {
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }
            let next_block = if let Some(block) = self.get_next_block() {
                block
            } else {
                continue;
            };

            let block_id = self.next_block_id();
            let block_descriptor = BuiltBlockDescriptorForSlotBidder::new(block_id, &next_block);

            let mut local_ctx = self.local_ctx();
            let mut block_building_helper = next_block.into_building_helper();
            if self.adjust_finalized_blocks {
		let value = if block_building_helper.true_block_value().unwrap_or_default().is_zero() {
		    U256::ZERO
		} else {
		    // set value to 1 so that some contracts do not revert
		    U256::ONE
		}; 
                match block_building_helper.finalize_block(&mut local_ctx, value, None) {
                    Ok(_) => {}
                    Err(err) => {
                        error!(?err, "Failed to prefinalize block");
                        continue;
                    }
                };
            }
            let prefinalized_result =
                PrefinalizedBlock::new(block_id, block_building_helper, local_ctx);
            self.finalized_blocks.lock().push(prefinalized_result);
            slot_bidder.notify_new_built_block(block_descriptor);
        }
    }

    fn get_next_finalize_command(&self) -> Option<FinalizeCommand> {
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

    fn seal_command(&self, bid: SlotBidderSealBidCommand) {
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
            let finalize_command = FinalizeCommand {
                prefinalized_block,
                value: bid.payout_tx_value,
                seen_competition_bid: bid.seen_competition_bid,
            };
            let (lock, cvar) = &*self.last_finalize_command;
            let mut guard = lock.lock();
            *guard = Some(finalize_command);
            cvar.notify_one();
        }
    }

    fn run_finalize_thread(self) {
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }
            let finalize_command = if let Some(command) = self.get_next_finalize_command() {
                command
            } else {
                continue;
            };

            let mut command = finalize_command.prefinalized_block.inner.lock();
            let result = match command.finalize_prefinalized_block(
                finalize_command.value,
                finalize_command.seen_competition_bid,
                self.adjust_finalized_blocks,
            ) {
                Ok(Some(result)) => result,
                Ok(None) => {
                    warn!("Prefinalized block was discarded");
                    continue;
                }
                Err(err) => {
                    error!(?err, "Failed to finalize prefinalized block");
                    continue;
                }
            };
            self.block_building_sink.lock().new_block(result.block);
        }
    }
}

impl BlockSealInterfaceForSlotBidder for UnfinishedBuiltBlocksInput {
    fn seal_bid(&self, bid: SlotBidderSealBidCommand) {
        self.seal_command(bid)
    }
}
