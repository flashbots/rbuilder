//! EPBS Bid Provider - Integrates with the block building pipeline to generate bids.
//!
//! This module provides the `LiveEpbsBidProvider` which implements `EpbsBidProvider`
//! by connecting to the existing block building infrastructure.

use alloy_primitives::{BlockHash, B256, U256};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{
    CachedPayloadData, ExecutionPayloadBid, ExecutionRequests, GetBidParams,
    SignedExecutionPayloadBid,
};
use std::{collections::HashMap, time::Instant};
use tracing::{debug, info, trace};

use crate::{
    building::builders::Block, live_builder::block_output::block_observer::BlockObserver,
    mev_boost::EpbsBidSigner,
};
use alloy_primitives::keccak256;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};

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
}

/// Config for the LiveEpbsBidProvider.
#[derive(Debug, Clone)]
pub struct LiveEpbsBidProviderConfig {
    /// max number of blocks to cache.
    pub max_cached_blocks: usize,
    /// max age of a cached block before it's considered stale.
    pub max_block_age_ms: u64,
}

impl Default for LiveEpbsBidProviderConfig {
    fn default() -> Self {
        Self {
            max_cached_blocks: 100,
            max_block_age_ms: 12_000, // one slot, but maybe we can also update it?
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
    signer: RwLock<Option<EpbsBidSigner>>,
    /// Best blocks by slot/parent key.
    best_blocks: RwLock<HashMap<SlotParentKey, CachedBlockData>>,
    /// Cache of full payloads for revelation, keyed by block_hash.
    payload_cache: RwLock<HashMap<BlockHash, CachedPayloadData>>,
}

impl LiveEpbsBidProvider {
    /// Create a new LiveEpbsBidProvider with a signer.
    pub fn new(signer: EpbsBidSigner, config: LiveEpbsBidProviderConfig) -> Self {
        Self {
            config,
            signer: RwLock::new(Some(signer)),
            best_blocks: RwLock::new(HashMap::new()),
            payload_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new uninitialized LiveEpbsBidProvider.
    ///
    /// The signer must be set later using `set_signer()` before bids can be generated.
    /// The builder_index will be obtained from the signer once it's set.
    pub fn new_uninitialized(config: LiveEpbsBidProviderConfig) -> Self {
        Self {
            config,
            signer: RwLock::new(None),
            best_blocks: RwLock::new(HashMap::new()),
            payload_cache: RwLock::new(HashMap::new()),
        }
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

    /// Notify the provider of a new built block.
    ///
    /// This should be called by the block building pipeline whenever a new
    /// block is produced. The provider will track the best block for each
    /// slot/parent combination.
    pub fn on_new_block(&self, slot: u64, parent_hash: BlockHash, block: Block) {
        let key = SlotParentKey { slot, parent_hash };

        let cached = CachedBlockData {
            block: block.clone(),
            cached_at: Instant::now(),
            slot,
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
    }

    /// Get the best block for a given slot/parent combination.
    pub fn get_best_block(&self, params: &GetBidParams) -> Option<CachedBlockData> {
        let key = SlotParentKey::from_params(params);
        let best_blocks = self.best_blocks.read();
        best_blocks.get(&key).cloned()
    }

    /// Convert a Block to an ExecutionPayloadBid.
    fn block_to_bid(
        block: &Block,
        params: &GetBidParams,
        builder_index: u64,
        blob_kzg_commitments_root: B256,
    ) -> ExecutionPayloadBid {
        // bid_value is in wei, we need gwei
        let value_gwei = (block.trace.bid_value / U256::from(1_000_000_000u64))
            .try_into()
            .unwrap_or(u64::MAX);

        ExecutionPayloadBid {
            parent_block_hash: params.parent_hash,
            parent_block_root: params.parent_root,
            block_hash: block.sealed_block.hash(),
            prev_randao: B256::ZERO, // TODO: fix this
            fee_recipient: params.fee_recipient,
            gas_limit: block.sealed_block.gas_limit,
            builder_index,
            slot: params.slot,
            value: value_gwei,
            execution_payment: 0, // In protocol payment
            blob_kzg_commitments_root,
        }
    }

    /// Compute the hash_tree_root of blob KZG commitments.
    ///
    /// In a full implementation, this would use SSZ merkleization.
    /// For now, we use a simplified version.
    fn compute_blob_commitments_root(&self, block: &Block) -> B256 {
        if block.txs_blobs_sidecars.is_empty() {
            return B256::ZERO;
        }

        // Collect all commitments
        let mut commitments_data = Vec::new();
        for sidecar in &block.txs_blobs_sidecars {
            match sidecar.as_ref() {
                alloy_eips::eip7594::BlobTransactionSidecarVariant::Eip4844(s) => {
                    for commitment in &s.commitments {
                        commitments_data.extend_from_slice(commitment.as_slice());
                    }
                }
                alloy_eips::eip7594::BlobTransactionSidecarVariant::Eip7594(s) => {
                    for commitment in &s.commitments {
                        commitments_data.extend_from_slice(commitment.as_slice());
                    }
                }
            }
        }

        if commitments_data.is_empty() {
            B256::ZERO
        } else {
            // Simplified: just hash the concatenated commitments
            // In production, use proper SSZ hash_tree_root
            keccak256(&commitments_data)
        }
    }

    /// Cache the payload for later revelation.
    fn cache_payload(&self, signed_bid: &SignedExecutionPayloadBid, block: &Block) {
        let block_hash = signed_bid.message.block_hash;

        // Convert block to ExecutionPayloadV3
        // This is a placeholder - in production you'd use proper conversion
        let payload = self.block_to_execution_payload(block);

        // Extract blob commitments
        let blob_kzg_commitments = self.extract_blob_commitments(block);

        let cached = CachedPayloadData::new(
            signed_bid.clone(),
            payload,
            ExecutionRequests::default(), // TODO: Convert from block.execution_requests
            blob_kzg_commitments,
        );

        self.payload_cache.write().insert(block_hash, cached);

        debug!(?block_hash, "Cached payload for revelation");
    }

    /// Convert a Block to ExecutionPayloadV3.
    ///
    /// TODO: Use proper conversion from rbuilder-primitives when available.
    fn block_to_execution_payload(&self, block: &Block) -> ExecutionPayloadV3 {
        let sealed = &block.sealed_block;

        // Extract transactions as raw bytes
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

        // Extract withdrawals
        let withdrawals = sealed
            .body()
            .withdrawals
            .as_ref()
            .map(|w| w.to_vec())
            .unwrap_or_default();

        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
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
                },
                withdrawals,
            },
            blob_gas_used: sealed.blob_gas_used.unwrap_or_default(),
            excess_blob_gas: sealed.excess_blob_gas.unwrap_or_default(),
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

    /// Get a cached payload by block hash.
    pub fn get_cached_payload(&self, block_hash: &BlockHash) -> Option<CachedPayloadData> {
        self.payload_cache.read().get(block_hash).cloned()
    }

    /// Cleanup stale cache entries.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let max_age_ms = self.config.max_block_age_ms as u128;

        self.best_blocks
            .write()
            .retain(|_, v| now.duration_since(v.cached_at).as_millis() < max_age_ms);

        self.payload_cache.write().retain(|_, v| {
            v.created_at.elapsed().as_millis() < max_age_ms * 2 // Keep payloads longer
        });
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
    fn on_block_built(&self, slot: u64, parent_hash: BlockHash, block: &Block) {
        self.on_new_block(slot, parent_hash, block.clone());
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

        // Compute blob commitments root
        let blob_commitments_root = self.compute_blob_commitments_root(&cached_block.block);

        // Create the bid
        let bid = Self::block_to_bid(
            &cached_block.block,
            params,
            signer.builder_index(),
            blob_commitments_root,
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
