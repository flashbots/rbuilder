use alloy_primitives::{Address, Bytes, B256};

/// The type representing UltraSound bid adjustments.
#[derive(
    PartialEq,
    Eq,
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
pub struct BidAdjustmentData {
    /// State root of the payload.
    pub state_root: B256,
    /// Transactions root of the payload.
    pub transactions_root: B256,
    /// Receipts root of the payload.
    pub receipts_root: B256,
    /// The usual builder address that pays the proposer in the last transaction of the block.
    /// When we adjust a bid, this transaction is overwritten by a transaction from the collateral
    /// account `fee_payer_address`. If we don't adjust the bid, `builder_address` pays the
    /// proposer as per usual.
    pub builder_address: Address,
    /// The state proof for the builder account.
    pub builder_proof: Vec<Bytes>,
    /// The proposer's fee recipient.
    pub fee_recipient_address: Address,
    /// The state proof for the fee recipient account.
    pub fee_recipient_proof: Vec<Bytes>,
    /// The fee payer address that is custodied by the relay.
    pub fee_payer_address: Address,
    /// The state proof for the fee payer account.
    pub fee_payer_proof: Vec<Bytes>,
    /// The merkle proof for the last transaction in the block, which will be overwritten with a
    /// payment from `fee_payer` to `fee_recipient` if we adjust the bid.
    pub placeholder_transaction_proof: Vec<Bytes>,
    /// The merkle proof for the receipt of the placeholder transaction. It's required for
    /// adjusting payments to contract addresses.
    pub placeholder_receipt_proof: Vec<Bytes>,
}
