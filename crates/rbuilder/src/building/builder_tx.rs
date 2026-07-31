//! Abstraction for the transactions the builder itself adds at the end of the block.
//!
//! Besides the orders it selects, a builder usually wants to append some transactions of
//! its own at the end of the block: a block signature, an attestation, a rebalance of the
//! coinbase account, and so on. [`BuilderTransactions`] is the extension point for those.
//!
//! Lifecycle, once per block:
//! 1. Before building starts, [`BuilderTransactions::validate`] and then
//!    [`BuilderTransactions::reserve_block_space`] are called on the state of the parent
//!    block (see [`crate::building::builders::block_building_helper::BlockBuildingHelperFromProvider::new_with_execution_tracer`]).
//!    The space every source asks for is held back for the whole build, so filling the
//!    block with orders can never leave the builder without room for its own txs, and the
//!    value it declares in [`BuilderTransactions::reserve_coinbase_value`] comes off the
//!    bid ceiling, since the builder account is what pays the proposer.
//! 2. At finalization, [`BuilderTransactions::transactions`] is called and the returned
//!    txs are committed after the accumulated refunds and before the proposer payout tx
//!    (see [`crate::building::PartialBlock::insert_refunds_and_proposer_payout_tx`]).
//!
//! The proposer payout tx is deliberately NOT modelled as a [`BuilderTransactions`]: it
//! has to stay the very last tx of the block (finalization, the receipts caches and the
//! backtest fee recipient detection all read it as `transactions.last()`) and its value is
//! only known once the bid is chosen, while builder txs are built once and then survive
//! every reseal of the block untouched.
//!
//! Registration is done on the building context with
//! [`BlockBuildingContext::with_builder_transactions`].

use super::{BlockBuildingContext, BlockState};
use crate::building::{cached_reads::CachedDB, BlockSpace};
use alloy_primitives::U256;
use rbuilder_primitives::TransactionSignedEcRecoveredWithBlobs;
use reth_errors::ProviderError;
use std::sync::Arc;

/// Errors reported by a [`BuilderTransactions`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum BuilderTransactionError {
    /// Reading the state of the parent block failed.
    #[error("Error accessing block state: {0}")]
    Provider(#[from] ProviderError),
    /// Signing a builder transaction failed.
    #[error("Failed to sign builder transaction: {0}")]
    Signing(#[from] secp256k1::Error),
    /// Anything specific to an implementation.
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl BuilderTransactionError {
    /// Wraps an implementation specific error.
    pub fn other(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(error))
    }

    /// Wraps an implementation specific error message.
    pub fn msg(msg: impl std::fmt::Display) -> Self {
        Self::Other(msg.to_string().into())
    }
}

/// What the builder holds back from the block for the txs it appends at the end of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndOfBlockReservation {
    /// Block space for the proposer payout tx and every registered builder tx.
    pub space: BlockSpace,
    /// Value the builder txs will move out of the builder account. Subtracted from the
    /// bid ceiling, since the payout to the proposer is paid from that same account.
    pub value: U256,
}

/// A source of transactions the builder appends at the end of the block, after the orders
/// and the accumulated refunds and before the proposer payout tx.
///
/// Implementations are registered per block via
/// [`BlockBuildingContext::with_builder_transactions`] and are shared across the building
/// threads, so they must be cheap to keep around and must not carry per-block mutable
/// state.
pub trait BuilderTransactions: std::fmt::Debug + Send + Sync {
    /// Name of this source, used in logs and errors. Should be stable.
    fn name(&self) -> &str;

    /// Called once before block building starts, on the state of the parent block.
    /// Returning an error aborts building the block, so this is the place to check that
    /// whatever the source needs (a funded account, a reachable contract, a configured
    /// address) is actually there instead of failing at finalization.
    fn validate(
        &self,
        _ctx: &BlockBuildingContext,
        _state: &mut BlockState<CachedDB>,
    ) -> Result<(), BuilderTransactionError> {
        Ok(())
    }

    /// Block space (gas, rlp length, blob gas) to hold back for the txs this source will
    /// emit. Called once, right after [`Self::validate`], on the state of the parent
    /// block.
    ///
    /// `space_reserved_so_far` is what the proposer payout tx and the previously
    /// registered sources already reserved, so an implementation that has to fit inside a
    /// budget can take it into account.
    ///
    /// The reserved space is subtracted from the block limits while orders are simulated
    /// and committed, and released just before the end of block txs are inserted. Txs
    /// that do not fit in the reserved space can still be rejected at finalization, so
    /// reserve for the worst case.
    fn reserve_block_space(
        &self,
        ctx: &BlockBuildingContext,
        state: &mut BlockState<CachedDB>,
        space_reserved_so_far: BlockSpace,
    ) -> Result<BlockSpace, BuilderTransactionError>;

    /// Value this source will move out of the builder account at finalization, on top of
    /// the gas its txs burn. Called once, right after [`Self::reserve_block_space`].
    ///
    /// The builder account (`ctx.builder_signer`) is also the block beneficiary and is
    /// what the proposer payout tx pays from, so anything a builder tx spends has to come
    /// off the bid ceiling: it is subtracted from
    /// [`crate::building::builders::block_building_helper::BlockBuildingHelper::true_block_value`].
    /// A source whose txs only burn gas can leave this at zero.
    fn reserve_coinbase_value(&self, _ctx: &BlockBuildingContext) -> U256 {
        U256::ZERO
    }

    /// The txs to append, in order. Called once per block, at finalization.
    ///
    /// `nonce` is the next free nonce of `ctx.builder_signer`; a source returning several
    /// txs must sign them with `nonce`, `nonce + 1`, ... All txs must be signed by
    /// `ctx.builder_signer`, since that is the only key the builder holds, and each of
    /// them must succeed: a reverted builder tx fails the whole block.
    ///
    /// This runs on the finalization path, which is latency critical and has no access to
    /// the block state, so any state read belongs in [`Self::validate`] or
    /// [`Self::reserve_block_space`].
    fn transactions(
        &self,
        ctx: &BlockBuildingContext,
        nonce: u64,
    ) -> Result<Vec<TransactionSignedEcRecoveredWithBlobs>, BuilderTransactionError>;
}

/// Validates every registered source and sums what they hold back, chaining the running
/// space total so each source sees what the previous ones reserved.
///
/// `payout_tx_space` is the space already reserved for the proposer payout tx.
pub fn reserve_end_of_block(
    builder_transactions: &[Arc<dyn BuilderTransactions>],
    ctx: &BlockBuildingContext,
    state: &mut BlockState<CachedDB>,
    payout_tx_space: BlockSpace,
) -> Result<EndOfBlockReservation, BuilderTransactionError> {
    builder_transactions.iter().try_fold(
        EndOfBlockReservation {
            space: payout_tx_space,
            value: U256::ZERO,
        },
        |reserved, builder_txs| {
            builder_txs.validate(ctx, state)?;
            builder_txs
                .reserve_block_space(ctx, state, reserved.space)
                .map(|space| EndOfBlockReservation {
                    space: reserved.space + space,
                    value: reserved.value + builder_txs.reserve_coinbase_value(ctx),
                })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::building::{
        create_payout_tx, payout_tx::tests::setup, FinalizeRevertStateCurrentIteration,
        InsertPayoutTxErr, PartialBlock, ThreadBlockBuildingContext,
    };
    use crate::utils::constants::BASE_TX_GAS;
    use alloy_primitives::U256;

    /// Test source emitting `count` zero value self transfers signed by the builder.
    #[derive(Debug)]
    struct SelfTransfers {
        count: u64,
    }

    impl BuilderTransactions for SelfTransfers {
        fn name(&self) -> &str {
            "self_transfers"
        }

        fn reserve_block_space(
            &self,
            _ctx: &BlockBuildingContext,
            _state: &mut BlockState<CachedDB>,
            _space_reserved_so_far: BlockSpace,
        ) -> Result<BlockSpace, BuilderTransactionError> {
            Ok(BlockSpace::new(BASE_TX_GAS * self.count, 0, 0))
        }

        fn transactions(
            &self,
            ctx: &BlockBuildingContext,
            nonce: u64,
        ) -> Result<Vec<TransactionSignedEcRecoveredWithBlobs>, BuilderTransactionError> {
            (0..self.count)
                .map(|index| {
                    let tx = create_payout_tx(
                        ctx.chain_spec.as_ref(),
                        ctx.evm_env.block_env.basefee,
                        &ctx.builder_signer,
                        nonce + index,
                        ctx.builder_signer.address,
                        BASE_TX_GAS,
                        U256::ZERO,
                    )?;
                    TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx)
                        .map_err(BuilderTransactionError::msg)
                })
                .collect()
        }
    }

    /// Test source that reports how much space was already reserved when it was asked.
    #[derive(Debug)]
    struct SpaceReporter {
        space: BlockSpace,
        value: U256,
        seen_reserved: std::sync::Mutex<Vec<BlockSpace>>,
    }

    impl SpaceReporter {
        fn new(gas: u64, value: u64) -> Self {
            Self {
                space: BlockSpace::new(gas, 0, 0),
                value: U256::from(value),
                seen_reserved: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl BuilderTransactions for SpaceReporter {
        fn name(&self) -> &str {
            "space_reporter"
        }

        fn reserve_coinbase_value(&self, _ctx: &BlockBuildingContext) -> U256 {
            self.value
        }

        fn reserve_block_space(
            &self,
            _ctx: &BlockBuildingContext,
            _state: &mut BlockState<CachedDB>,
            space_reserved_so_far: BlockSpace,
        ) -> Result<BlockSpace, BuilderTransactionError> {
            self.seen_reserved
                .lock()
                .expect("seen_reserved poisoned")
                .push(space_reserved_so_far);
            Ok(self.space)
        }

        fn transactions(
            &self,
            _ctx: &BlockBuildingContext,
            _nonce: u64,
        ) -> Result<Vec<TransactionSignedEcRecoveredWithBlobs>, BuilderTransactionError> {
            Ok(Vec::new())
        }
    }

    /// Test source emitting a tx that cannot fit in the block.
    #[derive(Debug)]
    struct DoesNotFit;

    impl BuilderTransactions for DoesNotFit {
        fn name(&self) -> &str {
            "does_not_fit"
        }

        fn reserve_block_space(
            &self,
            _ctx: &BlockBuildingContext,
            _state: &mut BlockState<CachedDB>,
            _space_reserved_so_far: BlockSpace,
        ) -> Result<BlockSpace, BuilderTransactionError> {
            Ok(BlockSpace::new(BASE_TX_GAS, 0, 0))
        }

        fn transactions(
            &self,
            ctx: &BlockBuildingContext,
            nonce: u64,
        ) -> Result<Vec<TransactionSignedEcRecoveredWithBlobs>, BuilderTransactionError> {
            // Way over the 30M block gas limit of the test context.
            create_payout_tx(
                ctx.chain_spec.as_ref(),
                ctx.evm_env.block_env.basefee,
                &ctx.builder_signer,
                nonce,
                ctx.builder_signer.address,
                40_000_000,
                U256::ZERO,
            )
            .map_err(BuilderTransactionError::from)
            .and_then(|tx| {
                TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx)
                    .map_err(BuilderTransactionError::msg)
            })
            .map(|tx| vec![tx])
        }
    }

    /// Test source failing validation.
    #[derive(Debug)]
    struct FailsValidation;

    impl BuilderTransactions for FailsValidation {
        fn name(&self) -> &str {
            "fails_validation"
        }

        fn validate(
            &self,
            _ctx: &BlockBuildingContext,
            _state: &mut BlockState<CachedDB>,
        ) -> Result<(), BuilderTransactionError> {
            Err(BuilderTransactionError::msg("nope"))
        }

        fn reserve_block_space(
            &self,
            _ctx: &BlockBuildingContext,
            _state: &mut BlockState<CachedDB>,
            _space_reserved_so_far: BlockSpace,
        ) -> Result<BlockSpace, BuilderTransactionError> {
            panic!("reserve_block_space must not be called when validation fails")
        }

        fn transactions(
            &self,
            _ctx: &BlockBuildingContext,
            _nonce: u64,
        ) -> Result<Vec<TransactionSignedEcRecoveredWithBlobs>, BuilderTransactionError> {
            unreachable!("transactions must not be called when validation fails")
        }
    }

    /// Inserts the end of block txs on a fresh PartialBlock and returns it.
    fn insert_end_of_block_txs(
        ctx: &BlockBuildingContext,
        state: &mut BlockState<CachedDB>,
        payout_value: U256,
    ) -> (
        PartialBlock<(), crate::building::NullPartialBlockExecutionTracer>,
        FinalizeRevertStateCurrentIteration,
    ) {
        let mut partial_block = PartialBlock::new(false);
        let mut revert_state = FinalizeRevertStateCurrentIteration::default();
        partial_block
            .insert_refunds_and_proposer_payout_tx(
                BASE_TX_GAS,
                payout_value,
                ctx,
                &mut ThreadBlockBuildingContext::default(),
                state,
                false,
                &mut revert_state,
            )
            .expect("end of block txs must be inserted");
        (partial_block, revert_state)
    }

    #[test]
    fn no_builder_transactions_inserts_only_the_payout_tx() {
        let (proposer, ctx, mut state) = setup(None, false);

        let (partial_block, _) = insert_end_of_block_txs(&ctx, &mut state, U256::from(1u64));

        assert_eq!(partial_block.executed_tx_infos.len(), 1);
        let payout = &partial_block.executed_tx_infos[0];
        assert_eq!(payout.tx.to(), Some(proposer));
        assert_eq!(payout.tx.nonce(), 0);
    }

    #[test]
    fn builder_txs_are_inserted_before_the_payout_tx() {
        let (proposer, ctx, mut state) = setup(None, false);
        let ctx = ctx.with_builder_transactions(vec![Arc::new(SelfTransfers { count: 2 })]);
        let builder = ctx.builder_signer.address;

        let (partial_block, _) = insert_end_of_block_txs(&ctx, &mut state, U256::from(1u64));

        let txs = &partial_block.executed_tx_infos;
        assert_eq!(txs.len(), 3);
        // Builder txs first, with consecutive nonces starting at the builder nonce.
        assert_eq!(txs[0].tx.to(), Some(builder));
        assert_eq!(txs[0].tx.nonce(), 0);
        assert_eq!(txs[1].tx.to(), Some(builder));
        assert_eq!(txs[1].tx.nonce(), 1);
        // The payout tx stays last, which the rest of finalization relies on.
        assert_eq!(txs[2].tx.to(), Some(proposer));
        assert_eq!(txs[2].tx.nonce(), 2);
        assert!(txs.iter().all(|tx| tx.receipt.success));
    }

    #[test]
    fn builder_txs_survive_a_reseal_untouched() {
        let (proposer, ctx, mut state) = setup(None, false);
        let ctx = ctx.with_builder_transactions(vec![Arc::new(SelfTransfers { count: 2 })]);
        let builder = ctx.builder_signer.address;

        let (mut partial_block, revert_state) =
            insert_end_of_block_txs(&ctx, &mut state, U256::from(1u64));

        // Reseal with a different bid: only the payout tx is reverted and rebuilt.
        partial_block.adjust_finalize_block_revert_to_prefinalized_state(revert_state, &mut state);
        assert_eq!(partial_block.executed_tx_infos.len(), 2);

        partial_block
            .insert_refunds_and_proposer_payout_tx(
                BASE_TX_GAS,
                U256::from(2u64),
                &ctx,
                &mut ThreadBlockBuildingContext::default(),
                &mut state,
                true,
                &mut FinalizeRevertStateCurrentIteration::default(),
            )
            .expect("resealed block must be inserted");

        let txs = &partial_block.executed_tx_infos;
        assert_eq!(txs.len(), 3);
        assert_eq!(txs[0].tx.to(), Some(builder));
        assert_eq!(txs[1].tx.to(), Some(builder));
        assert_eq!(txs[2].tx.to(), Some(proposer));
        // The rebuilt payout tx keeps the nonce the builder txs left it at.
        assert_eq!(txs[2].tx.nonce(), 2);
    }

    #[test]
    fn a_builder_tx_that_cannot_be_committed_fails_the_block() {
        let (_, ctx, mut state) = setup(None, false);
        let ctx = ctx.with_builder_transactions(vec![Arc::new(DoesNotFit)]);

        let mut partial_block = PartialBlock::new(false);
        let result = partial_block.insert_refunds_and_proposer_payout_tx(
            BASE_TX_GAS,
            U256::ZERO,
            &ctx,
            &mut ThreadBlockBuildingContext::default(),
            &mut state,
            false,
            &mut FinalizeRevertStateCurrentIteration::default(),
        );

        assert!(matches!(result, Err(InsertPayoutTxErr::TxErr(_))));
    }

    #[test]
    fn reserved_space_and_value_are_summed_and_chained() {
        let (_, ctx, mut state) = setup(None, false);
        let first = Arc::new(SpaceReporter::new(1_000, 7));
        let second = Arc::new(SpaceReporter::new(2_000, 11));
        let sources: Vec<Arc<dyn BuilderTransactions>> = vec![first.clone(), second.clone()];

        let payout_space = BlockSpace::new(BASE_TX_GAS, 100, 0);
        let reserved = reserve_end_of_block(&sources, &ctx, &mut state, payout_space)
            .expect("reservation must succeed");

        assert_eq!(reserved.space, BlockSpace::new(BASE_TX_GAS + 3_000, 100, 0));
        // Value comes off the bid ceiling, so it must be summed too.
        assert_eq!(reserved.value, U256::from(18u64));
        // Each source sees everything reserved before it.
        assert_eq!(
            *first.seen_reserved.lock().expect("poisoned"),
            vec![payout_space]
        );
        assert_eq!(
            *second.seen_reserved.lock().expect("poisoned"),
            vec![BlockSpace::new(BASE_TX_GAS + 1_000, 100, 0)]
        );
    }

    #[test]
    fn failed_validation_aborts_the_reservation() {
        let (_, ctx, mut state) = setup(None, false);
        let sources: Vec<Arc<dyn BuilderTransactions>> = vec![Arc::new(FailsValidation)];

        let result = reserve_end_of_block(&sources, &ctx, &mut state, BlockSpace::ZERO);

        assert!(matches!(result, Err(BuilderTransactionError::Other(_))));
    }
}
