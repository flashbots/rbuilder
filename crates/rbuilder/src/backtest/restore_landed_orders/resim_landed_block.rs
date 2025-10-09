use crate::{
    building::{
        tracers::AccumulatorSimulationTracer, BlockBuildingContext, BlockBuildingSpaceState,
        BlockState, PartialBlock, PartialBlockFork, ThreadBlockBuildingContext,
    },
    provider::StateProviderFactory,
    utils::{
        extract_onchain_block_txs, find_suggested_fee_recipient, mevblocker::get_mevblocker_price,
        signed_uint_delta, Signer,
    },
};
use ahash::{HashMap, HashSet};
use alloy_primitives::{TxHash, B256, I256};
use eyre::Context;
use rbuilder_primitives::evm_inspector::SlotKey;
use reth_chainspec::ChainSpec;
use reth_primitives::{Receipt, Recovered, TransactionSigned};
use std::sync::Arc;

#[derive(Debug)]
pub struct ExecutedTxs {
    tx: Recovered<TransactionSigned>,
    pub receipt: Receipt,
    pub coinbase_profit: I256,
    pub conflicting_txs: Vec<(B256, Vec<SlotKey>)>,
}

impl ExecutedTxs {
    pub fn hash(&self) -> TxHash {
        *self.tx.hash()
    }
}

pub fn sim_historical_block<P>(
    provider: P,
    chain_spec: Arc<ChainSpec>,
    onchain_block: alloy_rpc_types::Block,
) -> eyre::Result<Vec<ExecutedTxs>>
where
    P: StateProviderFactory,
{
    let mut results = Vec::new();

    let txs = extract_onchain_block_txs(&onchain_block)?;

    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);

    let coinbase = onchain_block.header.beneficiary;
    let parent_num_hash = onchain_block.header.parent_num_hash();

    let builder_signer = Signer::random(); // signer will not be used here as we just replay onchain transactions

    let mev_blocker_price =
        get_mevblocker_price(provider.history_by_block_hash(onchain_block.header.parent_hash)?)?;
    let ctx = BlockBuildingContext::from_onchain_block(
        onchain_block,
        chain_spec,
        None,
        HashSet::default(),
        coinbase,
        suggested_fee_recipient,
        builder_signer,
        Arc::from(provider.root_hasher(parent_num_hash)?),
        false,
        mev_blocker_price,
    );

    let mut local_ctx = ThreadBlockBuildingContext::default();

    let state_provider = provider.history_by_block_hash(ctx.attributes.parent)?;
    let mut partial_block = PartialBlock::new(true);
    let mut state = BlockState::new(state_provider);

    partial_block
        .pre_block_call(&ctx, &mut local_ctx, &mut state)
        .with_context(|| "Failed to pre_block_call")?;

    let mut space_state = BlockBuildingSpaceState::ZERO;
    let mut written_slots: HashMap<SlotKey, Vec<B256>> = HashMap::default();

    for (idx, tx) in txs.into_iter().enumerate() {
        let coinbase_balance_before = state.balance(
            coinbase,
            &ctx.shared_cached_reads,
            &mut local_ctx.cached_reads,
        )?;
        let mut accumulator_tracer = AccumulatorSimulationTracer::default();
        let result = {
            let mut fork = PartialBlockFork::new(&mut state, &ctx, &mut local_ctx)
                .with_tracer(&mut accumulator_tracer);
            fork.commit_tx(&tx, space_state)?
                .with_context(|| format!("Failed to commit tx: {} {:?}", idx, tx.hash()))?
        };
        let coinbase_balance_after = state.balance(
            coinbase,
            &ctx.shared_cached_reads,
            &mut local_ctx.cached_reads,
        )?;
        let coinbase_profit = signed_uint_delta(coinbase_balance_after, coinbase_balance_before);
        space_state.use_space(result.space_used());

        let mut conflicting_txs: HashMap<B256, Vec<SlotKey>> = HashMap::default();
        for (slot, _) in accumulator_tracer.used_state_trace.read_slot_values {
            if let Some(conflicting_txs_on_slot) = written_slots.get(&slot) {
                for conflicting_tx in conflicting_txs_on_slot {
                    conflicting_txs
                        .entry(*conflicting_tx)
                        .or_default()
                        .push(slot.clone());
                }
            }
        }

        for (slot, _) in accumulator_tracer.used_state_trace.written_slot_values {
            written_slots.entry(slot).or_default().push(tx.hash());
        }

        let conflicting_txs = {
            let mut res = conflicting_txs.into_iter().collect::<Vec<_>>();
            res.sort();
            for (_, slots) in &mut res {
                slots.sort();
                slots.dedup();
            }
            res
        };

        results.push(ExecutedTxs {
            tx: tx.into_internal_tx_unsecure(),
            receipt: result.tx_info.receipt,
            coinbase_profit,
            conflicting_txs,
        })
    }

    Ok(results)
}
