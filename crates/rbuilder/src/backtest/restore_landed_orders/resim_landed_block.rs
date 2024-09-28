use crate::building::evm_inspector::SlotKey;
use crate::building::tracers::AccumulatorSimulationTracer;
use crate::building::{BlockBuildingContext, BlockState, PartialBlock, PartialBlockFork};
use crate::utils::signed_uint_delta;
use crate::utils::{extract_onchain_block_txs, find_suggested_fee_recipient};
use ahash::{HashMap, HashSet};
use alloy_primitives::{B256, I256};
use eyre::Context;
use reth_chainspec::ChainSpec;
use reth_db::DatabaseEnv;
use reth_primitives::{Receipt, TransactionSignedEcRecovered, TxHash};
use revmc_toolkit_load::EvmCompilerFns;
use reth_provider::ProviderFactory;
use std::sync::Arc;

#[derive(Debug)]
pub struct ExecutedTxs {
    tx: TransactionSignedEcRecovered,
    pub receipt: Receipt,
    pub coinbase_profit: I256,
    pub conflicting_txs: Vec<(B256, Vec<SlotKey>)>,
}

impl ExecutedTxs {
    pub fn hash(&self) -> TxHash {
        self.tx.hash()
    }
}

pub fn sim_historical_block(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    chain_spec: Arc<ChainSpec>,
    onchain_block: alloy_rpc_types::Block,
    llvm_compiler_fns: Option<EvmCompilerFns>,
) -> eyre::Result<Vec<ExecutedTxs>> {
    let mut results = Vec::new();

    let txs = extract_onchain_block_txs(&onchain_block)?;

    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);
    let coinbase = onchain_block.header.miner;

    let ctx = BlockBuildingContext::from_onchain_block(
        onchain_block,
        chain_spec,
        None,
        HashSet::default(),
        coinbase,
        suggested_fee_recipient,
        None,
        llvm_compiler_fns,
    );

    let state_provider = provider_factory.history_by_block_hash(ctx.attributes.parent)?;
    let mut partial_block = PartialBlock::new(true, None);
    let mut state = BlockState::new(state_provider);

    partial_block
        .pre_block_call(&ctx, &mut state)
        .with_context(|| "Failed to pre_block_call")?;

    let mut cumulative_gas_used = 0;
    let mut cumulative_blob_gas_used = 0;
    let mut written_slots: HashMap<SlotKey, Vec<B256>> = HashMap::default();

    for (idx, tx) in txs.into_iter().enumerate() {
        let coinbase_balance_before = state.balance(coinbase)?;
        let mut accumulator_tracer = AccumulatorSimulationTracer::default();
        let result = {
            let mut fork = PartialBlockFork::new(&mut state).with_tracer(&mut accumulator_tracer);
            fork.commit_tx(&tx, &ctx, cumulative_gas_used, 0, cumulative_blob_gas_used)?
                .with_context(|| format!("Failed to commit tx: {} {:?}", idx, tx.hash()))?
        };
        let coinbase_balance_after = state.balance(coinbase)?;
        let coinbase_profit = signed_uint_delta(coinbase_balance_after, coinbase_balance_before);

        cumulative_gas_used += result.gas_used;
        cumulative_blob_gas_used += result.blob_gas_used;

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
            receipt: result.receipt,
            coinbase_profit,
            conflicting_txs,
        })
    }

    Ok(results)
}
