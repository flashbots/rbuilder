use alloy_primitives::TxHash;
use reth_transaction_pool::TransactionPool;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, EthPoolTransaction, ValidPoolTransaction,
};
use std::sync::Arc;

pub trait BuilderTransactionPool: Send + Sync + Clone {
    /// The transaction type of the pool
    type Transaction: EthPoolTransaction;

    /// Returns an iterator that yields transactions that are ready for block production with the
    /// given base fee and optional blob fee attributes.
    ///
    /// Consumer: Block production
    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>>;

    /// Removes all transactions corresponding to the given hashes.
    ///
    /// Consumer: Utility
    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;
}

impl<T: TransactionPool> BuilderTransactionPool for T {
    type Transaction = T::Transaction;

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        TransactionPool::best_transactions_with_attributes(self, best_transactions_attributes)
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        TransactionPool::remove_transactions(self, hashes)
    }
}
