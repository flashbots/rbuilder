//! EPBS Bid Provider - Integrates with the block building pipeline to generate bids.
//!
//! This module provides the `LiveEpbsBidProvider` which implements `EpbsBidProvider`
//! by connecting to the existing block building infrastructure.

use alloy_primitives::{BlockHash, B256, U256};
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{
    BeaconWithdrawal, CachedPayloadData, ExecutionPayloadBid, ExecutionRequests, GetBidParams,
    SignedExecutionPayloadBid,
};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

use crate::{
    building::builders::Block, live_builder::block_output::block_observer::BlockObserver,
    mev_boost::EpbsBidSigner,
};
use alloy_primitives::Bytes;

use super::EpbsBidProvider;

/// Key for tracking best blocks by slot and parent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlotParentKey {
    pub slot: u64,
    pub parent_hash: BlockHash,
}

impl SlotParentKey {
    pub fn from_params(params: &GetBidParams) -> Self {
        Self {
            slot: params.slot,
            parent_hash: params.parent_hash,
        }
    }
}

/// Cached block data for generating EPBS bids.
#[derive(Debug, Clone)]
pub struct CachedBlockData {
    /// The built block from the building pipeline.
    pub block: Block,
    /// When this block was cached.
    pub cached_at: Instant,
    /// Slot this block is for.
    pub slot: u64,
    /// Beacon parent_block_root from payload_attributes. MUST be paired with
    /// `parent_block_hash` (below) — Prysm verifies that the beacon block at
    /// `parent_block_root` commits to `parent_block_hash` as its EL payload.
    pub parent_block_root: B256,
    /// EL parent block hash from payload_attributes (NOT from the built block's
    /// own parent_hash — those can diverge from the CL's view).
    pub parent_block_hash: BlockHash,
}

/// Config for the LiveEpbsBidProvider.
#[derive(Debug, Clone)]
pub struct LiveEpbsBidProviderConfig {
    /// max number of blocks to cache.
    pub max_cached_blocks: usize,
    /// max age of a cached block before it's considered stale.
    pub max_block_age_ms: u64,
    /// only meant to be used for devnet testing
    /// TODO dont use for prod
    pub bid_value_subsidy_gwei: u64,
}

impl Default for LiveEpbsBidProviderConfig {
    fn default() -> Self {
        Self {
            max_cached_blocks: 100,
            max_block_age_ms: 12_000, // one slot, but maybe we can also update it?
            // Default 0: production-safe. Set explicitly via L1Config
            bid_value_subsidy_gwei: 0,
        }
    }
}

/// Live EPBS Bid Provider that integrates with the block building pipeline.
///
/// This provider:
/// 1. Receives built blocks from the block building pipeline
/// 2. Tracks the best block for each slot/parent combination
/// 3. Generates SignedExecutionPayloadBid on request
/// 4. Caches full payloads for later revelation
pub struct LiveEpbsBidProvider {
    /// Configuration.
    config: LiveEpbsBidProviderConfig,
    /// The signer for creating signed bids. Optional to support lazy initialization.
    /// Contains the builder_index (looked up from beacon chain by public key).
    signer: Arc<RwLock<Option<EpbsBidSigner>>>,
    /// Best blocks by slot/parent key.
    best_blocks: RwLock<HashMap<SlotParentKey, CachedBlockData>>,
    /// Cache of full payloads for revelation, keyed by **block_hash**.
    ///  growth is capped and  is achieved via slot-aware cleanup
    /// `cleanup_older_than(oldest_slot)` drops entries whose
    /// `bid.message.slot < oldest_slot`. The P2P main loop calls this on
    /// every detected slot transition with `current_slot - 64`, mirroring
    /// buildoor's pattern.
    payload_cache: Arc<RwLock<HashMap<BlockHash, CachedPayloadData>>>,
    fresh_block_tx: RwLock<Option<mpsc::UnboundedSender<u64>>>,
}

impl LiveEpbsBidProvider {
    /// Create a new LiveEpbsBidProvider with a signer.
    pub fn new(signer: EpbsBidSigner, config: LiveEpbsBidProviderConfig) -> Self {
        Self {
            config,
            signer: Arc::new(RwLock::new(Some(signer))),
            best_blocks: RwLock::new(HashMap::new()),
            payload_cache: Arc::new(RwLock::new(HashMap::new())),
            fresh_block_tx: RwLock::new(None),
        }
    }

    /// Create a new uninitialized LiveEpbsBidProvider.
    ///
    /// The signer must be set later using `set_signer()` before bids can be generated.
    /// The builder_index will be obtained from the signer once it's set.
    pub fn new_uninitialized(config: LiveEpbsBidProviderConfig) -> Self {
        Self {
            config,
            signer: Arc::new(RwLock::new(None)),
            best_blocks: RwLock::new(HashMap::new()),
            payload_cache: Arc::new(RwLock::new(HashMap::new())),
            fresh_block_tx: RwLock::new(None),
        }
    }

    /// Subscribe to "fresh block cached" notifications. Returns a receiver that
    /// emits a slot number whenever `on_new_block` caches a block (including
    /// when an existing slot's best block is updated).
    pub fn subscribe_fresh_blocks(&self) -> mpsc::UnboundedReceiver<u64> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.fresh_block_tx.write() = Some(tx);
        rx
    }

    /// Set the signer for this provider.
    ///
    /// This is used for lazy initialization when the builder_index and signing domain
    /// need to be fetched from the beacon chain after startup.
    pub fn set_signer(&self, signer: EpbsBidSigner) {
        *self.signer.write() = Some(signer);
    }

    /// Check if the signer is ready.
    pub fn is_ready(&self) -> bool {
        self.signer.read().is_some()
    }

    /// Get the builder index (if signer is initialized).
    pub fn builder_index(&self) -> Option<u64> {
        self.signer.read().as_ref().map(|s| s.builder_index())
    }

    /// Get a shared reference to the signer for use by the P2P service.
    pub fn shared_signer(&self) -> Arc<RwLock<Option<EpbsBidSigner>>> {
        self.signer.clone()
    }

    /// Get a shared reference to the payload cache for use by the P2P reveal handler.
    pub fn shared_payload_cache(&self) -> Arc<RwLock<HashMap<BlockHash, CachedPayloadData>>> {
        self.payload_cache.clone()
    }

    /// Notify the provider of a new built block.
    ///
    /// This should be called by the block building pipeline whenever a new
    /// block is produced. The provider will track the best block for each
    /// slot/parent combination.
    pub fn on_new_block(
        &self,
        slot: u64,
        parent_hash: BlockHash,
        parent_block_root: B256,
        block: Block,
    ) {
        let key = SlotParentKey { slot, parent_hash };

        let cached = CachedBlockData {
            block: block.clone(),
            cached_at: Instant::now(),
            slot,
            parent_block_root,
            // Use the parent_block_hash from payload_attributes (passed via
            // BlockObserver). It must match the EL block referenced by
            // `parent_block_root` from the CL's perspective. Using the built
            // block's own parent_hash can diverge from the CL view and breaks
            // Prysm's `VerifyParentBlockHash` check.
            parent_block_hash: parent_hash,
        };

        let mut best_blocks = self.best_blocks.write();

        // Check if this block is better than the current best
        let should_update = match best_blocks.get(&key) {
            Some(existing) => block.trace.bid_value > existing.block.trace.bid_value,
            None => true,
        };

        if should_update {
            info!(
                slot,
                ?parent_hash,
                block_hash = ?block.sealed_block.hash(),
                bid_value = %block.trace.bid_value,
                cached_blocks = best_blocks.len() + 1,
                "EPBS: Cached new best block for slot"
            );
            best_blocks.insert(key, cached);
        }

        // Cleanup old entries if we are over the limit
        if best_blocks.len() > self.config.max_cached_blocks {
            let now = Instant::now();
            best_blocks.retain(|_, v| {
                now.duration_since(v.cached_at).as_millis() < self.config.max_block_age_ms as u128
            });
        }

        // Release the best_blocks lock before notifying subscribers. Notifications
        // only fire when we actually updated the cache.
        drop(best_blocks);
        if should_update {
            if let Some(tx) = self.fresh_block_tx.read().as_ref() {
                // Channel is unbounded; failure means receiver dropped, which
                // is benign (the P2P service may not be running).
                let _ = tx.send(slot);
            }
        }
    }

    /// Get the best block for a given slot/parent combination.
    pub fn get_best_block(&self, params: &GetBidParams) -> Option<CachedBlockData> {
        let best_blocks = self.best_blocks.read();
        // If the caller specifies a concrete parent_hash, look up the exact key.
        // If parent_hash is ZERO (P2P bidder doesn't track parent_hash before
        // bidding), find the highest-value cached block for the requested slot.
        if !params.parent_hash.is_zero() {
            let key = SlotParentKey::from_params(params);
            return best_blocks.get(&key).cloned();
        }
        best_blocks
            .iter()
            .filter(|(k, _)| k.slot == params.slot)
            .max_by_key(|(_, v)| v.block.trace.bid_value)
            .map(|(_, v)| v.clone())
    }

    /// Convert a Block to an ExecutionPayloadBid.
    ///
    /// Method form (not associated fn) so it can read `self.config` for the
    /// devnet bid-value subsidy. See `LiveEpbsBidProviderConfig::bid_value_subsidy_gwei`
    /// for the rationale and removal criteria.
    fn block_to_bid(
        &self,
        cached: &CachedBlockData,
        params: &GetBidParams,
        builder_index: u64,
        blob_kzg_commitments: Vec<Bytes>,
    ) -> ExecutionPayloadBid {
        let block = &cached.block;
        // bid_value is in wei, we need gwei
        let true_value_gwei: u64 = (block.trace.bid_value / U256::from(1_000_000_000u64))
            .try_into()
            .unwrap_or(u64::MAX);

        // Apply the devnet subsidy on top of the block's true value. Saturating
        // add so a misconfigured huge subsidy doesn't wrap. When the subsidy is
        // 0 (production default), this is a no-op.
        let value_gwei = true_value_gwei.saturating_add(self.config.bid_value_subsidy_gwei);
        if self.config.bid_value_subsidy_gwei > 0 {
            debug!(
                slot = params.slot,
                true_value_gwei,
                subsidy_gwei = self.config.bid_value_subsidy_gwei,
                bid_value_gwei = value_gwei,
                "Applied devnet bid-value subsidy"
            );
        }

        // Use fee_recipient from caller (proposer preferences if cached, otherwise
        // the hardcoded devnet fallback set in p2p::service). If still zero (e.g.
        // builder server callers that don't set it), fall back to the block's own
        // beneficiary — this won't pass the gossip [REJECT] rule but is a sane
        // last resort.
        let fee_recipient = if !params.fee_recipient.is_zero() {
            params.fee_recipient
        } else {
            block.sealed_block.beneficiary
        };

        // If the caller didn't supply a concrete parent_hash (e.g. P2P bidder),
        // use the cached parent_block_hash from payload_attributes (NOT the built
        // block's own parent_hash). This must match the EL block that the beacon
        // block at `parent_block_root` commits to — Prysm verifies this pairing.
        let parent_block_hash = if params.parent_hash.is_zero() {
            cached.parent_block_hash
        } else {
            params.parent_hash
        };

        // Use the cached parent_block_root from when the block was prefinalized
        // (from payload_attributes_event for that slot). If caller supplied a
        // non-zero parent_root, that takes priority.
        let parent_block_root = if !params.parent_root.is_zero() {
            params.parent_root
        } else {
            cached.parent_block_root
        };

        // Compute the typed execution_requests root the bid commits to.
        // cl verifies at envelope reveal that the revealed requests hash
        // to this same root (gloas/p2p-interface.md)
        let exec_reqs = Self::convert_execution_requests(&block.execution_requests);
        let execution_requests_root = crate::mev_boost::sign_epbs::execution_requests_root(
            &exec_reqs,
        )
        .unwrap_or_else(|err| {
            tracing::warn!(
                slot = params.slot,
                ?err,
                "Failed to compute execution_requests_root; bidding with zero root \
                 (envelope reveal will fail cls request-root check)"
            );
            B256::ZERO
        });

        ExecutionPayloadBid {
            parent_block_hash,
            parent_block_root,
            block_hash: block.sealed_block.hash(),
            prev_randao: block.sealed_block.mix_hash,
            fee_recipient,
            gas_limit: block.sealed_block.gas_limit,
            builder_index,
            slot: params.slot,
            value: value_gwei,
            execution_payment: 0, // In protocol payment
            blob_kzg_commitments,
            execution_requests_root,
        }
    }

    // TODO: review implementation
    /// Convert execution requests from EIP-7685 typed format to separated lists.
    ///
    /// EIP-7685 execution requests are prefixed with a type byte:
    /// - 0x00: Deposit requests
    /// - 0x01: Withdrawal requests
    /// - 0x02: Consolidation requests
    fn convert_execution_requests(requests: &[Bytes]) -> ExecutionRequests {
        let mut deposits = Vec::new();
        let mut withdrawals = Vec::new();
        let mut consolidations = Vec::new();

        for request in requests {
            if request.is_empty() {
                continue;
            }

            let request_type = request[0];
            let request_data = Bytes::copy_from_slice(&request[1..]);

            match request_type {
                0x00 => deposits.push(request_data),
                0x01 => withdrawals.push(request_data),
                0x02 => consolidations.push(request_data),
                _ => {
                    // Unknown request type - skip it
                    debug!(request_type, "Unknown execution request type, skipping");
                }
            }
        }

        ExecutionRequests {
            deposits,
            withdrawals,
            consolidations,
        }
    }

    /// Cache the payload for later revelation.
    ///
    /// Keyed by block_hash so multiple bids per slot (different algos / different
    /// orderings → different block_hashes) all stay reachable; the proposer may
    /// pick any of them. Blob bytes are NOT copied here — `cached.sidecars`
    /// holds `Arc` refs to the original sidecars from the built block (cheap
    /// clone), and the wire-format `Vec<Bytes>` is built only at reveal time
    /// via `cached.blobs()` / `cached.cell_proofs()`.
    ///
    /// Bounded growth comes from slot-aware cleanup, not from the cache key.
    fn cache_payload(&self, signed_bid: &SignedExecutionPayloadBid, block: &Block) {
        let slot = signed_bid.message.slot;
        let block_hash = signed_bid.message.block_hash;

        // Convert block to ExecutionPayloadGloas
        let payload = self.block_to_execution_payload(block, slot);

        // Extract blob commitments
        let blob_kzg_commitments = self.extract_blob_commitments(block);

        // Hold the sidecars by Arc reference. No fresh blob byte copy at
        // cache-insert time — the original buffers in the Block stay shared.
        let sidecars: Vec<_> = block.txs_blobs_sidecars.to_vec();

        let execution_requests = Self::convert_execution_requests(&block.execution_requests);

        let cached = CachedPayloadData::new(
            signed_bid.clone(),
            payload,
            execution_requests,
            blob_kzg_commitments,
            sidecars,
        );

        debug!(
            slot,
            ?block_hash,
            sidecar_count = cached.sidecars.len(),
            "Cached payload for revelation"
        );

        self.payload_cache.write().insert(block_hash, cached);
    }

    /// Convert a Block to ExecutionPayloadGloas (flat snake_case wire shape).
    fn block_to_execution_payload(
        &self,
        block: &Block,
        slot: u64,
    ) -> rbuilder_primitives::epbs::ExecutionPayloadGloas {
        let sealed = &block.sealed_block;

        let transactions: Vec<Bytes> = sealed
            .body()
            .transactions
            .iter()
            .map(|tx| {
                let mut buf = Vec::new();
                alloy_eips::eip2718::Encodable2718::encode_2718(tx, &mut buf);
                Bytes::from(buf)
            })
            .collect();

        let withdrawals: Vec<BeaconWithdrawal> = sealed
            .body()
            .withdrawals
            .as_ref()
            .map(|w| {
                w.iter()
                    .map(|wd| BeaconWithdrawal {
                        index: wd.index,
                        validator_index: wd.validator_index,
                        address: wd.address,
                        amount: wd.amount,
                    })
                    .collect()
            })
            .unwrap_or_default();

        rbuilder_primitives::epbs::ExecutionPayloadGloas {
            parent_hash: sealed.parent_hash,
            fee_recipient: sealed.beneficiary,
            state_root: sealed.state_root,
            receipts_root: sealed.receipts_root,
            logs_bloom: sealed.logs_bloom,
            prev_randao: sealed.mix_hash,
            block_number: sealed.number,
            gas_limit: sealed.gas_limit,
            gas_used: sealed.gas_used,
            timestamp: sealed.timestamp,
            extra_data: sealed.extra_data.clone(),
            base_fee_per_gas: U256::from(sealed.base_fee_per_gas.unwrap_or_default()),
            block_hash: sealed.hash(),
            transactions,
            withdrawals,
            blob_gas_used: sealed.blob_gas_used.unwrap_or_default(),
            excess_blob_gas: sealed.excess_blob_gas.unwrap_or_default(),
            block_access_list: block.block_access_list.clone().unwrap_or_default(),
            slot_number: slot,
        }
    }

    /// Extract blob KZG commitments from a block.
    fn extract_blob_commitments(&self, block: &Block) -> Vec<alloy_primitives::Bytes> {
        let mut commitments = Vec::new();

        for sidecar in &block.txs_blobs_sidecars {
            match sidecar.as_ref() {
                alloy_eips::eip7594::BlobTransactionSidecarVariant::Eip4844(s) => {
                    for commitment in &s.commitments {
                        commitments.push(alloy_primitives::Bytes::copy_from_slice(
                            commitment.as_slice(),
                        ));
                    }
                }
                alloy_eips::eip7594::BlobTransactionSidecarVariant::Eip7594(s) => {
                    for commitment in &s.commitments {
                        commitments.push(alloy_primitives::Bytes::copy_from_slice(
                            commitment.as_slice(),
                        ));
                    }
                }
            }
        }

        commitments
    }

    /// Get a cached payload by block_hash.
    pub fn get_cached_payload(&self, block_hash: &BlockHash) -> Option<CachedPayloadData> {
        self.payload_cache.read().get(block_hash).cloned()
    }

    /// drop payload cache and best-blocks entries whose slot is strictly older
    /// than `oldest_slot_to_keep`. Called from the P2P main loop on slot
    /// transitions to bound memory growth payload_cache is keyed by block_hash
    /// but each entry knows its slot via `entry.bid.message.slot` so we filter on that.
    pub fn cleanup_older_than(&self, oldest_slot_to_keep: u64) {
        let mut payload = self.payload_cache.write();
        let before = payload.len();
        payload.retain(|_, v| v.bid.message.slot >= oldest_slot_to_keep);
        let after_payload = payload.len();
        drop(payload);

        let mut best = self.best_blocks.write();
        let best_before = best.len();
        best.retain(|key, _| key.slot >= oldest_slot_to_keep);
        let best_after = best.len();
        drop(best);

        if before != after_payload || best_before != best_after {
            debug!(
                oldest_slot_to_keep,
                payload_pruned = before - after_payload,
                best_pruned = best_before - best_after,
                "Pruned old cache entries"
            );
        }
    }
}

impl std::fmt::Debug for LiveEpbsBidProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveEpbsBidProvider")
            .field("config", &self.config)
            .field("builder_index", &self.builder_index())
            .field("signer_ready", &self.is_ready())
            .field("cached_blocks", &self.best_blocks.read().len())
            .field("cached_payloads", &self.payload_cache.read().len())
            .finish()
    }
}

/// Implement BlockObserver so that the block building pipeline can notify us of new blocks.
impl BlockObserver for LiveEpbsBidProvider {
    fn on_block_built(
        &self,
        slot: u64,
        parent_hash: BlockHash,
        parent_block_root: B256,
        block: &Block,
    ) {
        self.on_new_block(slot, parent_hash, parent_block_root, block.clone());
    }
}

#[async_trait::async_trait]
impl EpbsBidProvider for LiveEpbsBidProvider {
    async fn generate_bid(
        &self,
        params: &GetBidParams,
    ) -> eyre::Result<Option<SignedExecutionPayloadBid>> {
        // Check if signer is ready
        let signer = self.signer.read();
        let signer = match signer.as_ref() {
            Some(s) => s,
            None => {
                debug!(
                    slot = params.slot,
                    "EPBS signer not yet initialized, cannot generate bid"
                );
                return Ok(None);
            }
        };

        // Get the best block for this slot/parent
        let cached_block = match self.get_best_block(params) {
            Some(cached) => cached,
            None => {
                trace!(
                    slot = params.slot,
                    ?params.parent_hash,
                    "No block available for bid request"
                );
                return Ok(None);
            }
        };

        // Check if the block is too old
        let age_ms = cached_block.cached_at.elapsed().as_millis();
        if age_ms > self.config.max_block_age_ms as u128 {
            debug!(
                slot = params.slot,
                age_ms, "Best block is stale, not returning bid"
            );
            return Ok(None);
        }

        // extract blob commitments
        let blob_kzg_commitments = self.extract_blob_commitments(&cached_block.block);

        // Create the bid
        let bid = self.block_to_bid(
            &cached_block,
            params,
            signer.builder_index(),
            blob_kzg_commitments,
        );

        // Sign the bid
        let signed_bid = signer.sign_bid(&bid)?;

        info!(
            slot = params.slot,
            block_hash = ?signed_bid.message.block_hash,
            value = signed_bid.message.value,
            "Generated EPBS bid"
        );

        // Cache the payload for later revelation
        self.cache_payload(&signed_bid, &cached_block.block);

        Ok(Some(signed_bid))
    }
}
