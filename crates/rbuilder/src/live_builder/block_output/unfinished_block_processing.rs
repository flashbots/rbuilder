use std::{collections::VecDeque, time::Duration};

use alloy_primitives::{utils::format_ether, U256};
use derivative::Derivative;
use flume::RecvTimeoutError;
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;

use tracing::{error, info_span, trace, warn};

use ahash::HashMap;
use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        builders::block_building_helper::{BiddableUnfinishedBlock, BlockBuildingHelper},
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

#[derive(Debug, Clone)]
struct FinalizeWorkerInput {
    finalize_task: Arc<(Mutex<Option<FinalizeTask>>, Condvar)>,
}

impl FinalizeWorkerInput {
    fn new() -> Self {
        Self {
            finalize_task: Arc::new((Mutex::new(None), Condvar::new())),
        }
    }

    fn new_finalize_task(&self, finalize_task: FinalizeTask) {
        let (lock, cvar) = &*self.finalize_task;
        let mut guard = lock.lock();
        *guard = Some(finalize_task);
        cvar.notify_one();
    }

    fn wait_for_task(&self) -> Option<FinalizeTask> {
        let (lock, cvar) = &*self.finalize_task;
        let mut guard = lock.lock();
        while guard.is_none() {
            let timeout_result = cvar.wait_for(&mut guard, THREAD_BLOCKING_DURATION);
            if timeout_result.timed_out() {
                return None;
            }
        }
        guard.take()
    }
}

pub struct FinalizeWorker {
    slot_data: MevBoostSlotData,
    finalize_task: FinalizeWorkerInput,
    finalized_blocks: Box<dyn BlockBuildingSink>,
    cancellation_token: CancellationToken,
}

impl FinalizeWorker {
    pub fn run(self) {
        let slot_span = info_span!(
            "slot_data",
            payload_id = self.slot_data.payload_id,
            block = self.slot_data.block(),
            slot = self.slot_data.slot()
        );
        let _span_guard = slot_span.enter();

        let mut local_ctx = ThreadBlockBuildingContext::default();
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }

            let finalize_task = if let Some(task) = self.finalize_task.wait_for_task() {
                task
            } else {
                continue;
            };
            let FinalizeTask {
                block,
                payout_tx_val,
                seen_competition_bid,
            } = finalize_task;
            trace!(payout_tx_val = ?format_ether(payout_tx_val),
		   seen_competition_bid = ?seen_competition_bid.map(format_ether),
		   "Started block finalization");
            match block.finalize_block(&mut local_ctx, payout_tx_val, seen_competition_bid) {
                Ok(result) => {
                    self.finalized_blocks.new_block(result.block);
                }
                Err(err) => {
                    error!(?err, "Error finalizing block");
                }
            }
        }
        trace!("Shutting down finalize worker");
    }
}

enum BlockSealSlotWorkerCommands {
    NewBuiltBlock(BiddableUnfinishedBlock),
    SealBid(SlotBidderSealBidCommand),
}

#[derive(Clone, Debug)]
pub struct UnfinishedBuiltBlocksInput {
    command_queue: flume::Sender<BlockSealSlotWorkerCommands>,
}

pub struct BlockSealSlotWorkerOutput {
    command_queue: flume::Receiver<BlockSealSlotWorkerCommands>,
}

impl BlockSealInterfaceForSlotBidder for UnfinishedBuiltBlocksInput {
    fn seal_bid(&self, bid: SlotBidderSealBidCommand) {
        self.command_queue
            .send(BlockSealSlotWorkerCommands::SealBid(bid))
            .map_err(|err| warn!(?err, "Failed to send bid command"))
            .unwrap_or_default();
    }
}

impl UnfinishedBuiltBlocksInput {
    pub fn new_block(&self, block: BiddableUnfinishedBlock) {
        self.command_queue
            .send(BlockSealSlotWorkerCommands::NewBuiltBlock(block))
            .map_err(|err| warn!(?err, "Failed to send new block command"))
            .unwrap_or_default();
    }
}

fn create_slot_seal_worker() -> (UnfinishedBuiltBlocksInput, BlockSealSlotWorkerOutput) {
    let (sender, receiver) = flume::unbounded();
    (
        UnfinishedBuiltBlocksInput {
            command_queue: sender,
        },
        BlockSealSlotWorkerOutput {
            command_queue: receiver,
        },
    )
}

fn start_slot_seal_worker(
    slot_data: MevBoostSlotData,
    slot_bidder: Arc<dyn SlotBidder>,
    finalized_blocks: Box<dyn BlockBuildingSink>,
    output: BlockSealSlotWorkerOutput,
    built_block_cache: Arc<BuiltBlockCache>,
    cancellation_token: CancellationToken,
) {
    let finalize_task = FinalizeWorkerInput::new();
    let finalize_worker = FinalizeWorker {
        finalize_task: finalize_task.clone(),
        finalized_blocks,
        cancellation_token: cancellation_token.clone(),
        slot_data: slot_data.clone(),
    };

    let worker = UnfinishedBlocksSlotWorker {
        slot_data,
        cancellation_token,
        command_queue: output.command_queue,
        built_block_cache,
        last_block_by_algorithm: HashMap::default(),
        last_best_block_hash: 0,
        slot_bidder,
        pending_blocks_last_id: 0,
        pending_blocks: VecDeque::new(),
        finalize_worker: finalize_task.clone(),
    };
    std::thread::Builder::new()
        .name("unfinished_built_blocks_worker".into())
        .spawn(move || {
            if let Err(err) = worker.run() {
                error!(?err, "unfinished_built_blocks_worker exited with error");
            }
        })
        .expect("spawn block_seal_slot_worker");
    std::thread::Builder::new()
        .name("finalize_worker".into())
        .spawn(move || {
            finalize_worker.run();
        })
        .expect("spawn finalize_worker");
}

pub struct UnfinishedBlocksSlotWorker {
    slot_data: MevBoostSlotData,
    cancellation_token: CancellationToken,

    command_queue: flume::Receiver<BlockSealSlotWorkerCommands>,

    built_block_cache: Arc<BuiltBlockCache>,

    last_block_by_algorithm: HashMap<String, BiddableUnfinishedBlock>,
    last_best_block_hash: u64,

    slot_bidder: Arc<dyn SlotBidder>,

    // bidding service was notified about these blocks
    pending_blocks_last_id: u64,
    pending_blocks: VecDeque<(BlockId, Box<dyn BlockBuildingHelper>)>,

    finalize_worker: FinalizeWorkerInput,
}

#[derive(Derivative)]
#[derivative(Debug)]
struct FinalizeTask {
    #[derivative(Debug = "ignore")]
    block: Box<dyn BlockBuildingHelper>,
    payout_tx_val: U256,
    seen_competition_bid: Option<U256>,
}

impl UnfinishedBlocksSlotWorker {
    fn run(mut self) -> eyre::Result<()> {
        let slot_span = info_span!(
            "slot_data",
            payload_id = self.slot_data.payload_id,
            block = self.slot_data.block(),
            slot = self.slot_data.slot()
        );
        let _span_guard = slot_span.enter();

        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }
            let command = match self.command_queue.recv_timeout(THREAD_BLOCKING_DURATION) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break;
                }
            };
            match command {
                BlockSealSlotWorkerCommands::NewBuiltBlock(unfinished_block) => {
                    trace!(
                        builder_name = unfinished_block.block().builder_name(),
                        true_block_value = format_ether(unfinished_block.true_block_value),
                        "New unfinished block"
                    );

                    self.built_block_cache
                        .update_from_new_unfinished_block(unfinished_block.block());

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
                        continue;
                    }
                    self.last_best_block_hash = best_block_hash;
                    let new_block_id = BlockId(self.pending_blocks_last_id);
                    self.pending_blocks_last_id += 1;
                    self.pending_blocks
                        .push_back((new_block_id, last_best_block.block.box_clone()));
                    let block_descriptor =
                        BuiltBlockDescriptorForSlotBidder::new(new_block_id, last_best_block);
                    self.slot_bidder.notify_new_built_block(block_descriptor);
                }
                BlockSealSlotWorkerCommands::SealBid(slot_bidder_seal_bid_command) => {
                    trace!(?slot_bidder_seal_bid_command, "New seal bid command");
                    // prune blocks that are no longer useful
                    self.pending_blocks
                        .retain(|(id, _)| id.0 >= slot_bidder_seal_bid_command.block_id.0);
                    let block_to_seal = self.pending_blocks.iter().find_map(|(id, block)| {
                        if id == &slot_bidder_seal_bid_command.block_id {
                            Some(block)
                        } else {
                            None
                        }
                    });
                    let block = if let Some(block) = block_to_seal {
                        block.box_clone()
                    } else {
                        continue;
                    };
                    let finalize_task = FinalizeTask {
                        block,
                        payout_tx_val: slot_bidder_seal_bid_command.payout_tx_value,
                        seen_competition_bid: slot_bidder_seal_bid_command.seen_competition_bid,
                    };
                    self.finalize_worker.new_finalize_task(finalize_task);
                }
            }
        }
        trace!("Finished UnfinishedBuiltBlocksSlotWorker");

        Ok(())
    }
}

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
}

impl<P: StateProviderFactory> UnfinishedBuiltBlocksInputFactory<P> {
    pub fn new(
        bidding_service: Arc<dyn BiddingService>,
        block_sink_factory: RelaySubmitSinkFactory,
        wallet_balance_watcher: WalletBalanceWatcher<P>,
    ) -> Self {
        Self {
            bidding_service,
            block_sink_factory,
            wallet_balance_watcher,
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
        let (input, output) = create_slot_seal_worker();

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

        start_slot_seal_worker(
            slot_data,
            slot_bidder,
            finished_block_sink,
            output,
            built_block_cache,
            cancel,
        );

        input
    }
}
