use alloy_primitives::{BlockHash, B256};
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{CachedPayloadData, SignedExecutionPayloadBid};
use std::{collections::HashMap, sync::Arc};
use tracing::info;

use crate::{beacon_api_client::Client, mev_boost::EpbsBidSigner};

/// Handles payload revelation after bid inclusion in a beacon block.
#[derive(Clone)]
pub struct RevealHandler {
    /// Beacon api client for constructing and submitting envelopes.
    beacon_client: Client,
    /// Shared signer for envelope signing.
    signer: Arc<RwLock<Option<EpbsBidSigner>>>,
    /// Shared payload cache from the bid provider, keyed by block_hash.
    /// We key by block_hash (not slot) because we may submit multiple bids
    /// per slot — the proposer can include any of them, and we need to
    /// reveal the payload that backs the specific block_hash they picked.
    payload_cache: Arc<RwLock<HashMap<BlockHash, CachedPayloadData>>>,
}

impl RevealHandler {
    pub fn new(
        beacon_client: Client,
        signer: Arc<RwLock<Option<EpbsBidSigner>>>,
        payload_cache: Arc<RwLock<HashMap<BlockHash, CachedPayloadData>>>,
    ) -> Self {
        Self {
            beacon_client,
            signer,
            payload_cache,
        }
    }

    /// Called when a head event indicates our bid was included in a beacon block.
    pub async fn on_bid_included(
        &self,
        slot: u64,
        beacon_block_root: B256,
        parent_beacon_block_root: B256,
        included_bid: &SignedExecutionPayloadBid,
    ) -> eyre::Result<()> {
        let block_hash = included_bid.message.block_hash;

        info!(
            slot,
            ?block_hash,
            ?beacon_block_root,
            ?parent_beacon_block_root,
            builder_index = included_bid.message.builder_index,
            "Our bid was included, starting payload reveal"
        );

        // 1. look up the cached payload by the included bid's block_hash.
        // The cache holds an entry for every block_hash we ever bid for,
        // so even if the proposer included an earlier bid (not our latest),
        // we still find the right payload here. Bounded growth is enforced
        // by slot aware cleanup in the P2P main loop, not by overwriting on
        // the same key.
        let cached = self
            .payload_cache
            .read()
            .get(&block_hash)
            .cloned()
            .ok_or_else(|| {
                eyre::eyre!(
                    "No cached payload found for block_hash {:?} at slot {} \
                     (likely the matching bid was generated before cleanup retention)",
                    block_hash,
                    slot
                )
            })?;

        // 2. build envelope locally
        let envelope = cached.build_envelope(beacon_block_root, parent_beacon_block_root);

        // 3. sign the envelope
        let signed_envelope = {
            let signer_guard = self.signer.read();
            let signer = signer_guard
                .as_ref()
                .ok_or_else(|| eyre::eyre!("Signer not initialized"))?;
            signer.sign_envelope(&envelope)?
        };

        // Materialize raw blob bytes + cell proofs from the cached sidecars.
        // This is the only place we actually pay the per-blob heap copy cost
        let blobs = cached.blobs();
        let cell_proofs = cached.cell_proofs();

        info!(
            slot,
            ?block_hash,
            ?beacon_block_root,
            blob_count = blobs.len(),
            cell_proof_count = cell_proofs.len(),
            "Submitting signed execution payload envelope"
        );

        // 4. submit to p2p via beacon api, including raw blobs and
        // cell proofs so the beacon node can compute and broadcast
        // DataColumnSidecars. Without these, peers fail data availability
        // checks even though the envelope itself passes gossip validation.
        self.beacon_client
            .submit_execution_payload_envelope(&signed_envelope, &blobs, &cell_proofs)
            .await?;

        info!(
            slot,
            ?block_hash,
            "Successfully submitted execution payload envelope"
        );

        Ok(())
    }
}

impl std::fmt::Debug for RevealHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevealHandler")
            .field("beacon_client", &self.beacon_client)
            .finish()
    }
}
