//! Main P2P ePBS builder service orchestrator.
//!
//! This service coordinates the full P2P builder flow:
//! 1. Subscribes to SSE events from the beacon node (head, bids, proposer preferences)
//! 2. Submits bids to the beacon node for P2P gossip on a schedule
//! 3. Monitors for bid inclusion in beacon blocks
//! 4. Triggers payload envelope revelation after bid inclusion

use super::{
    bid_tracker::BidTracker,
    proposer_prefs::ProposerPreferencesCache,
    reveal_handler::RevealHandler,
    scheduler::BidScheduler,
    types::EpbsP2PConfig,
};
use crate::{
    beacon_api_client::{
        Client, ExecutionPayloadBidTopic, HeadEvent, HeadTopic, ProposerPreferencesTopic,
    },
    live_builder::builder_api::{EpbsBidProvider, LiveEpbsBidProvider},
    mev_boost::EpbsBidSigner,
};
use alloy_primitives::{BlockHash, B256};
use futures::StreamExt;
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{GetBidParams, SignedExecutionPayloadBid};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Main p2p epbs builder service.
/// This service coordinates the full P2P builder flow:
/// Noting all the duties it currently takes care of
/// 1. Subscribes to SSE events from the beacon node (head, bids, proposer preferences)
/// 2. Submits bids to the beacon node for P2P gossip on a schedule
/// 3. Monitors for bid inclusion in beacon blocks
/// 4. Triggers payload envelope revelation after bid inclusion
pub struct EpbsP2PService {
    config: EpbsP2PConfig,
    beacon_client: Client,
    bid_provider: Arc<LiveEpbsBidProvider>,
    signer: Arc<RwLock<Option<EpbsBidSigner>>>,
    payload_cache: Arc<RwLock<HashMap<BlockHash, rbuilder_primitives::epbs::CachedPayloadData>>>,
    /// Receiver for fresh block cached notifications from the bid provider.
    /// Consumed by the main loop to fire bids the moment a block is ready,
    /// bypassing the polling interval.
    fresh_block_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<u64>>>,
}

impl EpbsP2PService {
    pub fn new(
        config: EpbsP2PConfig,
        beacon_client: Client,
        bid_provider: Arc<LiveEpbsBidProvider>,
        signer: Arc<RwLock<Option<EpbsBidSigner>>>,
        payload_cache: Arc<
            RwLock<HashMap<BlockHash, rbuilder_primitives::epbs::CachedPayloadData>>,
        >,
    ) -> Self {
        let fresh_block_rx = bid_provider.subscribe_fresh_blocks();
        Self {
            config,
            beacon_client,
            bid_provider,
            signer,
            payload_cache,
            fresh_block_rx: tokio::sync::Mutex::new(Some(fresh_block_rx)),
        }
    }

    /// Run the P2P builder service.
    ///
    /// This spawns SSE listener tasks and runs the main event loop until cancelled.
    pub async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        info!("Starting EPBS P2P builder service");

        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if self.signer.read().is_some() {
                break;
            }
            debug!("Waiting for EPBS signer initialization...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        let builder_index = self
            .signer
            .read()
            .as_ref()
            .map(|s| s.builder_index())
            .unwrap_or(0);

        info!(builder_index, "EPBS P2P service signer ready");

        let scheduler = BidScheduler::new(&self.config);
        let bid_tracker = Arc::new(BidTracker::new(builder_index));
        let prefs_cache = Arc::new(ProposerPreferencesCache::new());
        let reveal_handler = RevealHandler::new(
            self.beacon_client.clone(),
            self.signer.clone(),
            self.payload_cache.clone(),
        );

        // channel for head events from sse listener
        // TODO: spinning unbounded channel for now. revist again.
        let (head_tx, mut head_rx) = mpsc::unbounded_channel::<HeadEvent>();
        // channel for bid events from sse listener
        // TODO: spinning unbounded channel for now. revist again.
        let (bid_tx, mut bid_rx) = mpsc::unbounded_channel::<SignedExecutionPayloadBid>();

        // spawning the sse listeners for head and bids
        let head_handle = {
            let client = self.beacon_client.clone();
            let cancel = cancel.clone();
            let tx = head_tx;
            tokio::spawn(async move {
                Self::run_head_listener(client, tx, cancel).await;
            })
        };

        let bid_handle = {
            let client = self.beacon_client.clone();
            let cancel = cancel.clone();
            let tx = bid_tx;
            tokio::spawn(async move {
                Self::run_bid_listener(client, tx, cancel).await;
            })
        };

        let prefs_handle = {
            let client = self.beacon_client.clone();
            let cancel = cancel.clone();
            let cache = prefs_cache.clone();
            tokio::spawn(async move {
                Self::run_prefs_listener(client, cache, cancel).await;
            })
        };

        // main event loop
        let mut current_slot = scheduler.current_slot();
        let mut bid_interval = self.create_bid_interval();

        let mut fresh_block_rx = self
            .fresh_block_rx
            .lock()
            .await
            .take()
            .expect("fresh_block_rx initialized in EpbsP2PService::new");

        info!(current_slot, "EPBS P2P service entering main loop");

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("EPBS P2P service shutting down");
                    break;
                }

                // head event recevied, check if our bid was included
                Some(head_event) = head_rx.recv() => {
                    let new_slot = head_event.slot;
                    if new_slot > current_slot {
                        debug!(old_slot = current_slot, new_slot, "New slot detected");
                        current_slot = new_slot;
                        bid_tracker.cleanup(current_slot);
                        //TODO keeping for 2 epochs think again about it.
                        prefs_cache.cleanup(current_slot, 64);
                        // we keep the most recent PAYLOAD_CACHE_RETENTION_SLOTS
                        // worth of entries, long enough to still reveal a
                        // bid that won a few slots ago, short enough to cap
                        // memory at PAYLOAD_CACHE_RETENTION_SLOTS × per-slot payload size.
                        const PAYLOAD_CACHE_RETENTION_SLOTS: u64 = 64;
                        let oldest_to_keep = current_slot
                            .saturating_sub(PAYLOAD_CACHE_RETENTION_SLOTS);
                        self.bid_provider.cleanup_older_than(oldest_to_keep);
                    }

                    // check if our bid was included in this block
                    self.handle_head_event(
                        &head_event,
                        &bid_tracker,
                        &reveal_handler,
                        builder_index,
                    ).await;
                }

                // competing bid received
                Some(bid) = bid_rx.recv() => {
                    let is_new_highest = bid_tracker.on_bid_received(&bid);
                    if is_new_highest && bid.message.builder_index != builder_index {
                        debug!(
                            slot = bid.message.slot,
                            competing_value = bid.message.value,
                            competing_builder = bid.message.builder_index,
                            "Outbid by competing builder"
                        );
                    }
                }

                // submit/resubmit bid
                _ = bid_interval.tick() => {
                    let next_slot = current_slot + 1;
                    let bid_slot = if scheduler.is_in_bidding_window(next_slot) {
                        Some(next_slot)
                    } else if scheduler.is_in_bidding_window(current_slot) {
                        Some(current_slot)
                    } else {
                        None
                    };

                    if let Some(slot) = bid_slot {
                        if let Err(e) = self.submit_bid(
                            slot,
                            &prefs_cache,
                            &bid_tracker,
                        ).await {
                            debug!(slot, error = %e, "Failed to submit bid");
                        }
                    }
                }

                Some(slot) = fresh_block_rx.recv() => {
                    if scheduler.is_in_bidding_window(slot) {
                        debug!(slot, "Fresh block ready and in window, submitting bid immediately");
                        if let Err(e) = self.submit_bid(
                            slot,
                            &prefs_cache,
                            &bid_tracker,
                        ).await {
                            debug!(slot, error = %e, "Failed to submit fresh-block bid");
                        }
                    } else {
                        let rel = scheduler.ms_relative_to_slot(slot);
                        debug!(
                            slot,
                            ms_relative_to_slot = rel,
                            "Fresh block ready but outside bid window; will be picked up by next interval tick"
                        );
                    }
                }
            }
        }

        // cleanup all 
        head_handle.abort();
        bid_handle.abort();
        prefs_handle.abort();

        info!("EPBS P2P service stopped");
        Ok(())
    }

    /// Handle a head event by checking if our bid was included.
    async fn handle_head_event(
        &self,
        head_event: &HeadEvent,
        bid_tracker: &BidTracker,
        reveal_handler: &RevealHandler,
        builder_index: u64,
    ) {
        let slot = head_event.slot;
        let block_root = head_event.block;

        let our_bid = bid_tracker.our_bid(slot);

        // query the beacon node to see which bid was included
        let block_root_hex = format!("0x{}", hex::encode(block_root));
        match self
            .beacon_client
            .get_beacon_block_bid(&block_root_hex)
            .await
        {
            Ok(Some(included_bid)) => {
                // trigger the reveal whenever the included bid is ours. We may
                // have submitted several bids per slot with different block hashes
                // the payload cache is keyed by block hash, so the RevealHandler 
                // will look up he included bids block hash directly.
                if included_bid.message.builder_index == builder_index {
                    if let Some(ref tracked) = our_bid {
                        if tracked.message.block_hash != included_bid.message.block_hash {
                            debug!(
                                slot,
                                included_block_hash = ?included_bid.message.block_hash,
                                latest_tracked_block_hash = ?tracked.message.block_hash,
                                "Proposer included one of our earlier bids (not our latest); revealing the included payload"
                            );
                        }
                    }
                    info!(
                        slot,
                        ?block_root,
                        included_block_hash = ?included_bid.message.block_hash,
                        "Our bid was included in the beacon block, triggering reveal"
                    );

                    if let Err(e) = reveal_handler
                        .on_bid_included(slot, block_root, &included_bid)
                        .await
                    {
                        error!(slot, error = %e, "Failed to reveal payload");
                    }
                } else {
                    debug!(
                        slot,
                        included_builder = included_bid.message.builder_index,
                        "Different builder's bid was included"
                    );
                }
            }
            Ok(None) => {
                debug!(slot, "No bid found in beacon block body");
            }
            Err(e) => {
                warn!(slot, error = %e, "Failed to query beacon block for bid");
            }
        }
    }

    /// Generate and submit a bid for the given slot.
    ///
    /// if proposer preferences are available, uses them for fee_recipient and validates
    /// the bid against them. If not available, falls back to using the payload's own
    /// values, which come from the suggested_fee_recipient in payload attributes.
    async fn submit_bid(
        &self,
        slot: u64,
        prefs_cache: &ProposerPreferencesCache,
        bid_tracker: &BidTracker,
    ) -> eyre::Result<()> {
        let prefs = prefs_cache.get(slot);
        let has_prefs = prefs.is_some();

        if !has_prefs {
            debug!(
                slot,
                "No proposer preferences for slot, falling back to payload values"
            );
        }

        // TODO: per consensus specs gloas/p2p-interface.md, `bid.fee_recipient`
        // MUST equal `ProposerPreferences.fee_recipient` for the slot. The proposer
        // signs and broadcasts these on the `proposer_preferences`
        // Once we receive prefs reliably via the SSE stream this hardcoded
        // fallback should be removed and bids should always use cached prefs.
        const DEVNET_FALLBACK_FEE_RECIPIENT: alloy_primitives::Address =
            alloy_primitives::address!("8943545177806ED17B9F23F0a21ee5948eCaa776");

        let fee_recipient = prefs
            .as_ref()
            .map(|p| p.fee_recipient)
            .unwrap_or(DEVNET_FALLBACK_FEE_RECIPIENT);

        let params = GetBidParams {
            slot,
            // bid provider uses the best cached blocks parent_hash
            parent_hash: BlockHash::ZERO, // this will be overridden by providers cached data
            // bid provider uses the cached parent_block_root captured from the
            // payload_attributes event when the block was built
            parent_root: B256::ZERO,
            proposer_index: prefs.as_ref().map(|p| p.validator_index).unwrap_or(0),
            fee_recipient,
            timeout_ms: None,
            date_milliseconds: None,
        };

        // generate bid via the bid provider
        let signed_bid = self
            .bid_provider
            .generate_bid(&params)
            .await?
            .ok_or_else(|| eyre::eyre!("No bid available for slot {}", slot))?;

        // validate P2P gossip rules per consensus-specs p2p rules
        // [REJECT] execution_payment must be 0 for P2P gossip
        if signed_bid.message.execution_payment != 0 {
            return Err(eyre::eyre!(
                "Bid has non-zero execution_payment ({}), cannot broadcast via P2P",
                signed_bid.message.execution_payment
            ));
        }

        // validate against proposer preferences only if available
        if let Some(ref prefs) = prefs {
            // [REJECT] fee_recipient must match proposer preferences
            if signed_bid.message.fee_recipient != prefs.fee_recipient {
                return Err(eyre::eyre!(
                    "Bid fee_recipient {:?} does not match proposer preferences {:?}",
                    signed_bid.message.fee_recipient,
                    prefs.fee_recipient
                ));
            }

            // [REJECT] gas_limit must match proposer preferences
            if signed_bid.message.gas_limit != prefs.gas_limit {
                return Err(eyre::eyre!(
                    "Bid gas_limit {} does not match proposer preferences {}",
                    signed_bid.message.gas_limit,
                    prefs.gas_limit
                ));
            }
        }

        // [REJECT] blob_kzg_commitments length must not exceed MAX_BLOB_COMMITMENTS_PER_BLOCK
        const MAX_BLOB_COMMITMENTS_PER_BLOCK: usize = 4096;
        if signed_bid.message.blob_kzg_commitments.len() > MAX_BLOB_COMMITMENTS_PER_BLOCK {
            return Err(eyre::eyre!(
                "Bid has {} blob_kzg_commitments, exceeds max {}",
                signed_bid.message.blob_kzg_commitments.len(),
                MAX_BLOB_COMMITMENTS_PER_BLOCK
            ));
        }

        info!(
            slot,
            value = signed_bid.message.value,
            block_hash = ?signed_bid.message.block_hash,
            builder_index = signed_bid.message.builder_index,
            "Submitting bid to beacon node for P2P gossip"
        );

        // submit to beacon node
        self.beacon_client
            .submit_execution_payload_bid(&signed_bid)
            .await?;

        // keep track of our bid
        bid_tracker.on_bid_submitted(&signed_bid);

        info!(
            slot,
            value = signed_bid.message.value,
            "Bid submitted successfully"
        );

        Ok(())
    }

    /// Create the bid resubmission interval timer.
    fn create_bid_interval(&self) -> tokio::time::Interval {
        let interval_ms = if self.config.bid_interval_ms > 0 {
            self.config.bid_interval_ms
        } else {
            // single bid mode: use a long interval, bids will be gated by the scheduler
            1000
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    /// SSE listener for head events.
    async fn run_head_listener(
        client: Client,
        tx: mpsc::UnboundedSender<HeadEvent>,
        cancel: CancellationToken,
    ) {
        loop {
            if cancel.is_cancelled() {
                return;
            }

            match client.get_events::<HeadTopic>().await {
                Ok(mut stream) => {
                    info!("Connected to beacon node head SSE stream");
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            event = stream.next() => {
                                match event {
                                    Some(Ok(head_event)) => {
                                        if tx.send(head_event).is_err() {
                                            return; // Receiver dropped
                                        }
                                    }
                                    Some(Err(e)) => {
                                        warn!(error = %e, "Error in head SSE stream");
                                        break; // Reconnect
                                    }
                                    None => {
                                        warn!("Head SSE stream ended");
                                        break; // Reconnect
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to connect to head SSE stream");
                }
            }

            // backoff before reconnecting
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    }

    /// SSE listener for execution payload bid events.
    async fn run_bid_listener(
        client: Client,
        tx: mpsc::UnboundedSender<SignedExecutionPayloadBid>,
        cancel: CancellationToken,
    ) {
        loop {
            if cancel.is_cancelled() {
                return;
            }

            match client.get_events::<ExecutionPayloadBidTopic>().await {
                Ok(mut stream) => {
                    info!("Connected to beacon node bid SSE stream");
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            event = stream.next() => {
                                match event {
                                    Some(Ok(bid)) => {
                                        if tx.send(bid).is_err() {
                                            return;
                                        }
                                    }
                                    Some(Err(e)) => {
                                        warn!(error = %e, "Error in bid SSE stream");
                                        break;
                                    }
                                    None => {
                                        warn!("Bid SSE stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to connect to bid SSE stream");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    }

    /// SSE listener for proposer preferences events.
    async fn run_prefs_listener(
        client: Client,
        cache: Arc<ProposerPreferencesCache>,
        cancel: CancellationToken,
    ) {
        loop {
            if cancel.is_cancelled() {
                return;
            }

            match client.get_events::<ProposerPreferencesTopic>().await {
                Ok(mut stream) => {
                    info!("Connected to beacon node proposer preferences SSE stream");
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            event = stream.next() => {
                                match event {
                                    Some(Ok(signed_prefs)) => {
                                        cache.insert(signed_prefs.message);
                                    }
                                    Some(Err(e)) => {
                                        warn!(error = %e, "Error in proposer preferences SSE stream");
                                        break;
                                    }
                                    None => {
                                        warn!("Proposer preferences SSE stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to connect to proposer preferences SSE stream");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    }
}

impl std::fmt::Debug for EpbsP2PService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpbsP2PService")
            .field("config", &self.config)
            .finish()
    }
}
