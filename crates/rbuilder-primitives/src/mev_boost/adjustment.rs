use alloy_primitives::{Address, Bloom, Bytes, B256};

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
pub struct BidAdjustmentDataV1 {
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
pub struct BidAdjustmentDataV2 {
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
    /// New in V2: SSZ merkle proof for last transaction
    pub cl_placeholder_transaction_proof: Vec<B256>,
    /// The merkle proof for the receipt of the placeholder transaction. It's required for
    /// adjusting payments to contract addresses.
    pub placeholder_receipt_proof: Vec<Bytes>,
    /// New in V2: Logs bloom accrued until but not including the last (payment) transaction.
    pub pre_payment_logs_bloom: Bloom,
}

/// Common bid adjustment information that can be used for creating bid adjustment data.
#[derive(Clone, Debug)]
pub struct BidAdjustmentData {
    /// State root of the payload.
    pub state_root: B256,
    /// Transactions root of the payload.
    pub el_transactions_root: B256,
    /// Withdrawals root of the payload.
    pub el_withdrawals_root: B256,
    /// Receipts root of the payload.
    pub receipts_root: B256,
    /// The merkle proof for the last transaction in the block, which will be overwritten with a
    /// payment from `fee_payer` to `fee_recipient` if we adjust the bid.
    pub el_placeholder_transaction_proof: Vec<Bytes>,
    /// New in V2: SSZ merkle proof for last transaction
    pub cl_placeholder_transaction_proof: Vec<B256>,
    /// The merkle proof for the receipt of the placeholder transaction. It's required for
    /// adjusting payments to contract addresses.
    pub placeholder_receipt_proof: Vec<Bytes>,
    /// New in V2: Logs bloom accrued until but not including the last (payment) transaction.
    pub pre_payment_logs_bloom: Bloom,
    /// State proofs.
    pub state_proofs: BidAdjustmentStateProofs,
}

impl BidAdjustmentData {
    /// Convert bid adjustment data into [`BidAdjustmentDataV1`].
    pub fn into_v1(self) -> BidAdjustmentDataV1 {
        BidAdjustmentDataV1 {
            state_root: self.state_root,
            transactions_root: self.el_transactions_root,
            receipts_root: self.receipts_root,
            builder_address: self.state_proofs.builder_address,
            builder_proof: self.state_proofs.builder_proof,
            fee_recipient_address: self.state_proofs.fee_recipient_address,
            fee_recipient_proof: self.state_proofs.fee_recipient_proof,
            fee_payer_address: self.state_proofs.fee_payer_address,
            fee_payer_proof: self.state_proofs.fee_payer_proof,
            placeholder_transaction_proof: self.el_placeholder_transaction_proof,
            placeholder_receipt_proof: self.placeholder_receipt_proof,
        }
    }

    /// Convert bid adjustment data into [`BidAdjustmentDataV2`].
    pub fn into_v2(self) -> BidAdjustmentDataV2 {
        BidAdjustmentDataV2 {
            el_transactions_root: self.el_transactions_root,
            el_withdrawals_root: self.el_withdrawals_root,
            builder_address: self.state_proofs.builder_address,
            builder_proof: self.state_proofs.builder_proof,
            fee_recipient_address: self.state_proofs.fee_recipient_address,
            fee_recipient_proof: self.state_proofs.fee_recipient_proof,
            fee_payer_address: self.state_proofs.fee_payer_address,
            fee_payer_proof: self.state_proofs.fee_payer_proof,
            el_placeholder_transaction_proof: self.el_placeholder_transaction_proof,
            cl_placeholder_transaction_proof: self.cl_placeholder_transaction_proof,
            placeholder_receipt_proof: self.placeholder_receipt_proof,
            pre_payment_logs_bloom: self.pre_payment_logs_bloom,
        }
    }
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
