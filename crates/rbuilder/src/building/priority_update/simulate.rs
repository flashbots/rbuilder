use crate::building::{
    cached_reads::CachedDB, create_sim_value, BlockBuildingContext, BlockBuildingSpaceState,
    BlockState, CriticalCommitOrderError, PartialBlockFork, ThreadBlockBuildingContext,
};
use ahash::HashSet;
use alloy_primitives::Address;
use rbuilder_primitives::{Order, SimulatedOrder};
use reth_provider::StateProvider;
use revm::database::{states::PlainStorageChangeset, OriginalValuesKnown};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info_span};

pub struct PurSimulationResult {
    pub simulated_order: Arc<SimulatedOrder>,
    pub changeset: Vec<PlainStorageChangeset>,
}

pub fn simulate_priority_update(
    order: Arc<Order>,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_block_state_provider: Arc<dyn StateProvider>,
) -> Result<Option<PurSimulationResult>, CriticalCommitOrderError> {
    let first_tx = order.list_txs().into_iter().next();
    let from = first_tx.as_ref().map(|(tx, _)| tx.signer());
    let to = first_tx.as_ref().and_then(|(tx, _)| tx.to());
    let _span =
        info_span!("simulate_priority_update", order_id = ?order.id(), ?from, ?to).entered();

    let cached = CachedDB::new(parent_block_state_provider, ctx.shared_cached_reads.clone());
    let mut state = BlockState::boxed(cached);

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

    let order_ok = match result {
        Ok(ok) => ok,
        Err(err) => {
            debug!(
                ?err,
                reason = "simulation failed",
                "priority update discarded"
            );
            return Ok(None);
        }
    };

    let sim_value = create_sim_value(&order, &order_ok, &ctx.mempool_tx_detector);
    let used_state_trace = order_ok.used_state_trace.clone();
    let simulated_order = Arc::new(SimulatedOrder::new(
        Arc::clone(&order),
        sim_value,
        used_state_trace,
    ));

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
        return Ok(None);
    }

    changeset
        .storage
        .retain(|s| s.address != coinbase && !senders.contains(&s.address));

    if changeset.storage.is_empty() {
        debug!(
            reason = "empty after filtering",
            "priority update discarded"
        );
        return Ok(None);
    }

    if changeset.storage.iter().any(|s| s.wipe_storage) {
        debug!(reason = "wipe_storage", "priority update discarded");
        return Ok(None);
    }

    Ok(Some(PurSimulationResult {
        simulated_order,
        changeset: changeset.storage,
    }))
}
