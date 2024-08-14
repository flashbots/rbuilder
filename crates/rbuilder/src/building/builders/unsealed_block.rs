use alloy_primitives::U256;
use reth_payload_builder::database::CachedReads;
use reth_provider::StateProviderBox;

use crate::building::{
    tracers::GasUsedSimulationTracer, BlockBuildingContext, BlockState, PartialBlock,
};

/// Block under construction. It still needs to be finished by setting the payout tx and computing some extra stuff (eg: root hash).
/// Txs can still be added before finishing it.
/// Name still being decided.
struct UnsealedBlock {
    /// Balance of fee recipient before we stared building.
    /// Used to compute
    fee_recipient_balance_start: U256,

    state_cached_reads: CachedReads,
    state_bundle_state: Option<BundleState>,

    partial_block: PartialBlock<GasUsedSimulationTracer>,
    payout_tx_gas: Option<u64>,
    /// Name of the builder that pregenerated this block.
    /// Might be ambiguous if several building parts were involved...
    builder_name: String,
    building_ctx: BlockBuildingContext,
}
impl UnsealedBlock {
    pub fn new(state_provider: IntBox) -> Self {
        let state = BlockState2::new(&state_provider);
        Self {
            fee_recipient_balance_start: Default::default(),
            block_state: state,
            //            partial_block: todo!(),
            payout_tx_gas: Default::default(),
            builder_name: Default::default(),
        }
    }
}
