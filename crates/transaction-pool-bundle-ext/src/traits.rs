//! [`TransactionPoolBundleExt`] implementation generic over any bundle and network type.

use reth_primitives::B256;
use reth_transaction_pool::TransactionPool;
use std::fmt::Debug;

/// Extension for [TransactionPool] trait adding support for [BundlePoolOperations].
pub trait TransactionPoolBundleExt: TransactionPool + BundlePoolOperations {}

/// Bundle-related operations.
///
/// This API is under active development.
pub trait BundlePoolOperations: Sync + Send {
    /// Bundle type
    type Bundle;

    /// Error type
    type Error: Debug;

    /// Transactions type
    type Transaction;

    /// Add a bundle to the pool, returning an Error if invalid.
    fn add_bundle(&self, bundle: Self::Bundle) -> Result<(), Self::Error>;

    /// Make a best-effort attempt to cancel a bundle
    fn cancel_bundle(&self, hash: &B256) -> Result<(), Self::Error>;

    /// Get transactions to be included in the head of the next block
    fn get_transactions(&self) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error>;
}
