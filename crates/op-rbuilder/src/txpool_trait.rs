use alloy_primitives::TxHash;
use reth_transaction_pool::TransactionPool;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, BlobStore, EthPoolTransaction, Pool,
    PoolTransaction, TransactionOrdering, TransactionValidator, ValidPoolTransaction,
};
use std::sync::Arc;
#[auto_impl::auto_impl(&, Arc)]
pub trait CustomTransactionPool: Send + Sync + Clone {
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

#[derive(Debug)]
pub struct CustomPool<Pool>
where
    Pool: TransactionPool,
    Pool::Transaction: PoolTransaction,
{
    pool: Arc<Pool>,
}

impl<Pool> CustomPool<Pool>
where
    Pool: TransactionPool,
    Pool::Transaction: PoolTransaction,
{
    pub fn new(pool: Pool) -> Self {
        Self {
            pool: Arc::new(pool),
        }
    }
}

impl<Pool> CustomTransactionPool for CustomPool<Pool>
where
    Pool: TransactionPool,
    Pool::Transaction: PoolTransaction,
{
    type Transaction = Pool::Transaction;

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        TransactionPool::best_transactions_with_attributes(&self.pool, best_transactions_attributes)
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        TransactionPool::remove_transactions(&self.pool, hashes)
    }
}

impl<Pool: TransactionPool> Clone for CustomPool<Pool> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
        }
    }
}
