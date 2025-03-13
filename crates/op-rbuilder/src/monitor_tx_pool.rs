use futures_util::StreamExt;
use reth_optimism_node::txpool::OpPooledTransaction;
use reth_transaction_pool::{AllTransactionsEvents, FullTransactionEvent};
use tracing::debug;

pub async fn monitor_tx_pool(mut new_transactions: AllTransactionsEvents<OpPooledTransaction>) {
    while let Some(event) = new_transactions.next().await {
        transaction_event_log(event);
    }
}

fn transaction_event_log(event: FullTransactionEvent<OpPooledTransaction>) {
    match event {
        FullTransactionEvent::Pending(hash) => {
            debug!("Transaction event: tx={:?}, kind=pending", hash)
        }
        FullTransactionEvent::Queued(hash) => {
            debug!("Transaction event: tx={:?}, kind=queued", hash)
        }
        FullTransactionEvent::Mined {
            tx_hash,
            block_hash,
        } => debug!(
            "Transaction event: tx={:?}, kind=mined, block={:?}",
            tx_hash, block_hash
        ),
        FullTransactionEvent::Replaced {
            transaction,
            replaced_by,
        } => debug!(
            "Transaction event: tx={:?}, kind=replaced, replaced_by={:?}",
            transaction.hash(),
            replaced_by
        ),
        FullTransactionEvent::Discarded(hash) => {
            debug!("Transaction event: tx={:?}, kind=discarded", hash)
        }
        FullTransactionEvent::Invalid(hash) => {
            debug!("Transaction event: tx={:?}, kind=invalid", hash)
        }
        FullTransactionEvent::Propagated(_propagated) => {}
    }
}
