pub mod interfaces;
pub mod sequential_sealer_bid_maker;
pub mod true_block_value_bidder;

use alloy_primitives::U256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealInstruction {
    /// Don't waste cycles sealing block that has no chances
    Skip,
    /// Set this value in the last tx before sealing block
    Value(U256),
}
