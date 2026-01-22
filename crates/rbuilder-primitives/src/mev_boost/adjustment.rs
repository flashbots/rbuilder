use alloy_primitives::{Address, Bloom, Bytes, B256};

/// The type for bid adjustments in optimistic v3.
/// Ref: <https://github.com/ultrasoundmoney/docs/blob/main/optimistic-v3.md#optimistic-v3>
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
pub struct BidAdjustmentDataV3 {
    /// Transactions root of the payload.
    pub el_transactions_root: B256,
    /// Withdrawals root of the payload.
    pub el_withdrawals_root: B256,
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
    pub el_placeholder_transaction_proof: Vec<Bytes>,
    /// SSZ merkle proof for last transaction
    pub cl_placeholder_transaction_proof: Vec<B256>,
    /// The merkle proof for the receipt of the placeholder transaction. It's required for
    /// adjusting payments to contract addresses.
    pub el_placeholder_receipt_proof: Vec<Bytes>,
    /// Logs bloom accrued until but not including the last (payment) transaction.
    pub pre_payment_logs_bloom: Bloom,
    /// Gas used by the placeholder (payout) transaction. Required for V3 to relax the
    /// gas_limit == gas_used requirement.
    pub placeholder_gas_used: u64,
}

/// Bid adjustment state proofs.
#[derive(Clone, Debug)]
pub struct BidAdjustmentStateProofs {
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
}
