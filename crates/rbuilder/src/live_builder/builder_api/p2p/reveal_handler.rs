use alloy_primitives::{BlockHash, B256};
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{CachedPayloadData, SignedExecutionPayloadBid};
use std::{collections::HashMap, sync::Arc};
use tracing::{info, warn};

use crate::{beacon_api_client::Client, mev_boost::EpbsBidSigner};

/// Handles payload revelation after bid inclusion in a beacon block.
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
        included_bid: &SignedExecutionPayloadBid,
    ) -> eyre::Result<()> {
        let block_hash = included_bid.message.block_hash;

        info!(
            slot,
            ?block_hash,
            ?beacon_block_root,
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

        // 2. construct envelope via beacon node
        // retrying up to 3 times before giving up.
        //(TODO: check if the PR gets merged which computes state_root).
        let envelope = {
            let mut result = None;
            for attempt in 1..=3 {
                match self
                    .beacon_client
                    .construct_execution_payload_envelope(
                        beacon_block_root,
                        &cached.payload,
                        &cached.execution_requests,
                    )
                    .await
                {
                    Ok(envelope) => {
                        info!(
                            slot,
                            ?block_hash,
                            state_root = ?envelope.state_root,
                            "Beacon node constructed envelope with state_root"
                        );
                        result = Some(envelope);
                        break;
                    }
                    Err(e) => {
                        warn!(
                            slot,
                            ?block_hash,
                            attempt,
                            error = %e,
                            "Failed to construct envelope via beacon node, retrying..."
                        );
                        if attempt < 3 {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
            result.ok_or_else(|| {
                eyre::eyre!(
                    "Failed to construct envelope after 3 attempts for slot {}. \
                     Cannot reveal without valid state_root.",
                    slot,
                )
            })?
        };

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
        // (it happens once per won slot, not once per generated bid).
        // TODO: rethink this plis, am not too confident about this
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
