use crate::{
    building::{
        builders::block_building_helper::{
            BlockBuildingHelper, BlockBuildingHelperError, FinalizeBlockResult,
        },
        ThreadBlockBuildingContext,
    },
    live_builder::block_output::relay_submit::BlockBuildingSink,
};
use alloy_primitives::U256;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::interfaces::{Bid, BidMaker};

/// BidMaker with a background task sealing only one bid at a time.
/// If several bids arrive while sealing another one we keep only the last one since we assume new is better.
#[derive(Debug)]
pub struct SequentialSealerBidMaker {
    pending_bid: Arc<PendingBid>,
}

impl BidMaker for SequentialSealerBidMaker {
    fn send_bid(&self, bid: Bid) {
        self.pending_bid.update(bid);
    }
}

/// Object used to send new bids to the [SequentialSealerBidMakerProcess].
#[derive(Debug)]
struct PendingBid {
    /// Next bid to send.
    bid: Mutex<Option<Bid>>,
    /// Signaled when we set a new bid.
    bid_notify: Notify,
}

impl PendingBid {
    fn new() -> Self {
        Self {
            bid: Default::default(),
            bid_notify: Notify::new(),
        }
    }
    pub async fn wait_for_change(&self) {
        self.bid_notify.notified().await
    }
    /// Updates bid, replacing  on current (we assume they are always increasing but we don't check it).
    fn update(&self, bid: Bid) {
        *self.bid.lock() = Some(bid);
        self.bid_notify.notify_one();
    }

    fn consume_bid(&self) -> Option<Bid> {
        self.bid.lock().take()
    }
}

impl SequentialSealerBidMaker {
    pub fn new(sink: Arc<dyn BlockBuildingSink>, cancel: CancellationToken) -> Self {
        let pending_bid = Arc::new(PendingBid::new());
        let (sender, receiver) = flume::unbounded();
        let mut sealing_process = SequentialSealerBidMakerProcess {
            sink,
            cancel,
            pending_bid: pending_bid.clone(),
            worker_tasks: sender,
        };

        tokio::task::spawn(async move {
            sealing_process.run().await;
        });

        std::thread::Builder::new()
            .name("finalize_worker".into())
            .spawn(move || {
                run_finalize_worker(receiver);
            })
            .expect("spawn finalize_worker");

        Self { pending_bid }
    }
}

/// Background task waiting for new bids to seal.
struct SequentialSealerBidMakerProcess {
    /// Destination of the finished blocks.
    sink: Arc<dyn BlockBuildingSink>,
    cancel: CancellationToken,
    pending_bid: Arc<PendingBid>,
    worker_tasks: flume::Sender<FinalizeTask>,
}

impl SequentialSealerBidMakerProcess {
    async fn run(&mut self) {
        loop {
            tokio::select! {
                _ = self.pending_bid.wait_for_change() => self.check_for_new_bid().await,
                _ = self.cancel.cancelled() => return
            }
        }
    }

    /// block.finalize_block + self.sink.new_block inside spawn_blocking.
    async fn check_for_new_bid(&mut self) {
        if let Some(bid) = self.pending_bid.consume_bid() {
            let payout_tx_val = bid.payout_tx_value();
            let seen_competition_bid = bid.seen_competition_bid();
            let block = bid.block();
            let block_number = block.building_context().block();
            let builder_name = block.builder_name().to_string();

            let (result_sender, result_receiver) = flume::unbounded();
            let task = FinalizeTask {
                block,
                payout_tx_val,
                seen_competition_bid,
                result_sender,
            };
            match self.worker_tasks.send_async(task).await {
                Ok(()) => {}
                Err(err) => {
                    error!(
                        ?err,
                        "Error sending finalize_block task to the worker thread"
                    );
                    return;
                }
            }
            let finalize_res = match result_receiver.recv_async().await {
                Ok(ok) => ok,
                Err(err) => {
                    error!(
                        ?err,
                        "Error receiving finalize_block task from the worker thread"
                    );
                    return;
                }
            };

            match finalize_res {
                Ok(res) => self.sink.new_block(res.block),
                Err(err) => {
                    if err.is_critical() {
                        error!(
                            builder_name,
                            block = block_number,
                            ?err,
                            "Error on finalize_block on SequentialSealerBidMaker"
                        )
                    }
                }
            }
        }
    }
}

struct FinalizeTask {
    block: Box<dyn BlockBuildingHelper>,
    payout_tx_val: Option<U256>,
    seen_competition_bid: Option<U256>,

    result_sender: flume::Sender<Result<FinalizeBlockResult, BlockBuildingHelperError>>,
}

// run finalize worken in a separate thread so we can keep local ctx
fn run_finalize_worker(tasks: flume::Receiver<FinalizeTask>) {
    let mut local_ctx = ThreadBlockBuildingContext::default();
    loop {
        let FinalizeTask {
            block,
            payout_tx_val,
            seen_competition_bid,
            result_sender,
        } = match tasks.recv() {
            Ok(task) => task,
            Err(flume::RecvError::Disconnected) => {
                break;
            }
        };

        let result = block.finalize_block(&mut local_ctx, payout_tx_val, seen_competition_bid);
        let _ = result_sender.send(result);
    }
}
