use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::live_builder::block_output::relay_submit::BlockBuildingSink;

use super::interfaces::{Bid, BidMaker};

/// BidMaker with a background task sealing multiple parallel bids concurrently.
/// If several bids arrive while we hit the max of concurrent sealings we keep only the last one since we assume new is better.
#[derive(Debug)]
pub struct ParallelSealerBidMaker {
    pending_bid: Arc<PendingBid>,
}

impl BidMaker for ParallelSealerBidMaker {
    fn send_bid(&self, bid: Bid) {
        self.pending_bid.update(bid);
    }
}

/// Object used to send new bids to the [ParallelSealerBidMakerProcess].
#[derive(Debug)]
struct PendingBid {
    /// Next bid to send.
    bid: Mutex<Option<Bid>>,
    /// Signaled when we set a new bid.
    bid_notify: Arc<Notify>,
}

impl PendingBid {
    fn new(bid_notify: Arc<Notify>) -> Self {
        Self {
            bid: Default::default(),
            bid_notify,
        }
    }
    /// Updates bid, replacing  on current (we assume they are always increasing but we don't check it).
    fn update(&self, bid: Bid) {
        let mut current_bid = self.bid.lock().unwrap();
        *current_bid = Some(bid);
        self.bid_notify.notify_one();
    }

    fn consume_bid(&self) -> Option<Bid> {
        let mut current_bid = self.bid.lock().unwrap();
        current_bid.take()
    }
}

impl ParallelSealerBidMaker {
    pub fn new(
        max_concurrent_seals: usize,
        sink: Arc<dyn BlockBuildingSink>,
        cancel: CancellationToken,
    ) -> Self {
        let notify = Arc::new(Notify::new());
        let pending_bid = Arc::new(PendingBid::new(notify.clone()));
        let mut sealing_process = ParallelSealerBidMakerProcess {
            sink,
            cancel,
            pending_bid: pending_bid.clone(),
            notify: notify.clone(),
            seal_control: Arc::new(SealsInProgress {
                notify,
                seals_in_progress: Default::default(),
            }),
            max_concurrent_seals,
        };

        tokio::task::spawn(async move {
            sealing_process.run().await;
        });
        Self { pending_bid }
    }
}

struct SealsInProgress {
    /// Signaled when a sealing finishes.
    notify: Arc<Notify>,
    /// Number of current sealings in progress.
    seals_in_progress: Mutex<usize>,
}

/// Background task waiting for new bids to seal.
struct ParallelSealerBidMakerProcess {
    /// Destination of the finished blocks.
    sink: Arc<dyn BlockBuildingSink>,
    cancel: CancellationToken,
    pending_bid: Arc<PendingBid>,
    /// Signaled when we set a new bid or a sealing finishes.
    notify: Arc<Notify>,
    /// Shared between the sealing tasks and the main loop.
    seal_control: Arc<SealsInProgress>,
    /// Maximum number of concurrent sealings.
    max_concurrent_seals: usize,
}

impl ParallelSealerBidMakerProcess {
    async fn run(&mut self) {
        loop {
            tokio::select! {
                _ = self.wait_for_change() => self.check_for_new_bid().await,
                _ = self.cancel.cancelled() => return
            }
        }
    }

    async fn wait_for_change(&self) {
        self.notify.notified().await
    }

    /// block.finalize_block + self.sink.new_block inside spawn_blocking.
    async fn check_for_new_bid(&mut self) {
        if *self.seal_control.seals_in_progress.lock().unwrap() >= self.max_concurrent_seals {
            return;
        }
        if let Some(bid) = self.pending_bid.consume_bid() {
            let payout_tx_val = bid.payout_tx_value();
            let block = bid.block();
            let block_number = block.building_context().block();
            // Take sealing "slot"
            *self.seal_control.seals_in_progress.lock().unwrap() += 1;
            let seal_control = self.seal_control.clone();
            let sink = self.sink.clone();
            tokio::task::spawn_blocking(move || {
                match block.finalize_block(payout_tx_val) {
                    Ok(res) => sink.new_block(res.block),
                    Err(error) => error!(
                        block_number,
                        ?error,
                        "Error on finalize_block on ParallelSealerBidMaker"
                    ),
                };
                // release sealing "slot"
                *seal_control.seals_in_progress.lock().unwrap() -= 1;
                seal_control.notify.notify_one();
            });
        }
    }
}
