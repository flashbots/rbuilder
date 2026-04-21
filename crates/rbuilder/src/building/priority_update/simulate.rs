use crate::building::{
    BlockBuildingContext, BlockBuildingSpaceState, BlockState, CriticalCommitOrderError,
    PartialBlockFork, ThreadBlockBuildingContext,
};
use ahash::HashSet;
use alloy_primitives::Address;
use rbuilder_primitives::Order;
use reth_provider::StateProvider;
use revm::database::{states::PlainStorageChangeset, OriginalValuesKnown};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info_span};

pub fn simulate_priority_update(
    order: Arc<Order>,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_block_state_provider: Arc<dyn StateProvider>,
) -> Result<Vec<PlainStorageChangeset>, CriticalCommitOrderError> {
    let first_tx = order.list_txs().into_iter().next();
    let from = first_tx.as_ref().map(|(tx, _)| tx.signer());
    let to = first_tx.as_ref().and_then(|(tx, _)| tx.to());
    let _span =
        info_span!("simulate_priority_update", order_id = ?order.id(), ?from, ?to).entered();

    let mut state = BlockState::new_arc(parent_block_state_provider);

    let combined_refunds = HashMap::default();
    let result = {
        let mut fork = PartialBlockFork::new(&mut state, ctx, local_ctx);
        fork.commit_order(
            &order,
            BlockBuildingSpaceState::ZERO,
            true,
            &combined_refunds,
        )?
    };

    if let Err(err) = result {
        debug!(
            ?err,
            reason = "simulation failed",
            "priority update discarded"
        );
        return Ok(Vec::new());
    }

    let (bundle_state, _) = state.into_parts();

    let coinbase = ctx.evm_env.block_env.beneficiary;
    let senders: HashSet<Address> = order
        .list_txs()
        .into_iter()
        .map(|(tx, _)| tx.signer())
        .collect();

    let mut changeset = bundle_state.to_plain_state(OriginalValuesKnown::Yes);

    if !changeset.contracts.is_empty() {
        debug!(
            reason = "changeset contains contracts",
            "priority update discarded"
        );
        return Ok(Vec::new());
    }

    changeset
        .storage
        .retain(|s| s.address != coinbase && !senders.contains(&s.address));

    if changeset.storage.is_empty() {
        debug!(
            reason = "empty after filtering",
            "priority update discarded"
        );
        return Ok(Vec::new());
    }

    if changeset.storage.iter().any(|s| s.wipe_storage) {
        debug!(reason = "wipe_storage", "priority update discarded");
        return Ok(Vec::new());
    }

    Ok(changeset.storage)
}
