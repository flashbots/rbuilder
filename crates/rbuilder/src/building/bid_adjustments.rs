use alloy_eips::Encodable2718 as _;
use alloy_primitives::{Address, B256};
use rbuilder_primitives::mev_boost::{
    ssz_roots::{tx_ssz_leaf_root, CompactSszTransactionTree},
    BidAdjustmentStateProofs,
};
use reth_primitives::TransactionSigned;
use std::collections::{HashMap, HashSet};
use tracing::*;

use crate::building::{
    BlockBuildingContext, BlockState, FinalizeError, ThreadBlockBuildingContext,
    TransactionSszLeafRootCache,
};

/// Generate bid adjustment state proofs.
pub fn generate_bid_adjustment_state_proofs(
    block_state: &mut BlockState,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
) -> Result<HashMap<Address, BidAdjustmentStateProofs>, FinalizeError> {
    if ctx.adjustment_fee_payers.is_empty() {
        return Ok(Default::default());
    }

    let builder_signer = &ctx.builder_signer;
    let builder_address = builder_signer.address;
    let fee_recipient_address = ctx.attributes.suggested_fee_recipient;

    let proof_targets = HashSet::from_iter(
        [builder_address, fee_recipient_address]
            .into_iter()
            .chain(ctx.adjustment_fee_payers.clone()),
    );

    let mut account_proofs =
        ctx.root_hasher
            .account_proofs(block_state.bundle_state(), &proof_targets, local_ctx)?;

    let Some(builder_proof) = account_proofs.remove(&builder_address) else {
        return Err(FinalizeError::Other(eyre::eyre!(
            "account proof for builder {builder_address} is missing"
        )));
    };
    let Some(fee_recipient_proof) = account_proofs.remove(&fee_recipient_address) else {
        return Err(FinalizeError::Other(eyre::eyre!(
            "account proof for proposer {fee_recipient_address} is missing"
        )));
    };

    let mut bid_adjustments = HashMap::default();
    for fee_payer_address in &ctx.adjustment_fee_payers {
        let Some(fee_payer_proof) = account_proofs.remove(fee_payer_address) else {
            error!(
                %fee_payer_address,
                "Fee payer proof is missing"
            );
            continue;
        };

        bid_adjustments.insert(
            *fee_payer_address,
            BidAdjustmentStateProofs {
                builder_address,
                builder_proof: builder_proof.clone(),
                fee_recipient_address,
                fee_recipient_proof: fee_recipient_proof.clone(),
                fee_payer_address: *fee_payer_address,
                fee_payer_proof,
            },
        );
    }

    Ok(bid_adjustments)
}

/// Compute the SSZ transaction proof for bid adjustments.
pub fn compute_cl_placeholder_transaction_proof(
    transactions: &[TransactionSigned],
    cache: &TransactionSszLeafRootCache,
) -> Vec<B256> {
    let mut buf = Vec::new();
    let mut leaves = Vec::with_capacity(transactions.len());
    let mut cache = cache.lock();
    for tx in transactions {
        let leaf = if let Some(leaf) = cache.get(tx.hash()) {
            *leaf
        } else {
            buf.clear();
            tx.encode_2718(&mut buf);
            let leaf_root = tx_ssz_leaf_root(&buf);
            cache.insert(*tx.hash(), leaf_root);
            leaf_root
        };
        leaves.push(leaf);
    }
    let target = transactions.len().checked_sub(1).unwrap();
    CompactSszTransactionTree::from_leaves(leaves).proof(target)
}
