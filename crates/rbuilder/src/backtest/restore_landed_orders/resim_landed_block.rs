use crate::building::{BlockBuildingContext, BlockState, PartialBlock, PartialBlockFork};
use crate::primitives::serialize::{RawTx, TxEncoding};
use crate::primitives::TransactionSignedEcRecoveredWithBlobs;
use crate::utils::signed_uint_delta;
use ahash::HashSet;
use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, I256};
use eyre::Context;
use reth_chainspec::ChainSpec;
use reth_db::DatabaseEnv;
use reth_primitives::{Receipt, TransactionSignedEcRecovered};
use reth_provider::ProviderFactory;
use std::sync::Arc;

#[derive(Debug)]
pub struct ExecutedTxs {
    pub tx: TransactionSignedEcRecovered,
    pub receipt: Receipt,
    pub coinbase_profit: I256,
}

pub fn sim_historical_block(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    chain_spec: Arc<ChainSpec>,
    onchain_block: alloy_rpc_types::Block,
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
    );

    let state_provider = provider_factory.history_by_block_hash(ctx.attributes.parent)?;
    let mut partial_block = PartialBlock::new(true, None);
    let mut state = BlockState::new(&state_provider);

    partial_block
        .pre_block_call(&ctx, &mut state)
        .with_context(|| "Failed to pre_block_call")?;

    let mut cumulative_gas_used = 0;
    let mut cumulative_blob_gas_used = 0;
    for (idx, tx) in txs.into_iter().enumerate() {
        let coinbase_balance_before = state.balance(coinbase)?;
        let result = {
            let mut fork = PartialBlockFork::new(&mut state);
            fork.commit_tx(&tx, &ctx, cumulative_gas_used, 0, cumulative_blob_gas_used)?
                .with_context(|| format!("Failed to commit tx: {} {:?}", idx, tx.hash()))?
        };
        let coinbase_balance_after = state.balance(coinbase)?;
        let coinbase_profit = signed_uint_delta(coinbase_balance_after, coinbase_balance_before);

        cumulative_gas_used += result.gas_used;
        cumulative_blob_gas_used += result.blob_gas_used;

        results.push(ExecutedTxs {
            tx: tx.tx,
            receipt: result.receipt,
            coinbase_profit,
        })
    }

    Ok(results)
}

fn find_suggested_fee_recipient(
    block: &alloy_rpc_types::Block,
    txs: &[TransactionSignedEcRecoveredWithBlobs],
) -> Address {
    let coinbase = block.header.miner;
    let (last_tx_signer, last_tx_to) = if let Some((signer, to)) = txs
        .last()
        .map(|tx| (tx.tx.signer(), tx.tx.to().unwrap_or_default()))
    {
        (signer, to)
    } else {
        return coinbase;
    };

    if last_tx_signer == coinbase {
        last_tx_to
    } else {
        coinbase
    }
}

fn extract_onchain_block_txs(
    onchain_block: &alloy_rpc_types::Block,
) -> eyre::Result<Vec<TransactionSignedEcRecoveredWithBlobs>> {
    let mut result = Vec::new();
    for tx in onchain_block.transactions.clone().into_transactions() {
        let tx_envelope: TxEnvelope = tx.try_into()?;
        let encoded = tx_envelope.encoded_2718();
        let tx = RawTx { tx: encoded.into() }.decode(TxEncoding::NoBlobData)?;
        result.push(tx.tx_with_blobs);
    }
    Ok(result)
}
