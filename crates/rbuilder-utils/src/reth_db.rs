use std::path::Path;

use reth_provider::{providers::RocksDBProvider, ProviderResult};

/// Opens reth's RocksDB instance read-only.
///
/// reth keeps RocksDB open read-write for the node's lifetime, so a separate process (rbuilder or
/// an offline harness) must open it read-only to avoid the exclusive lock, the same way it opens
/// MDBX and static files. `with_default_tables` registers the column families reth writes (history
/// indices, tx-hash lookups) so reads resolve against the on-disk data.
pub fn open_rocksdb_read_only(rocksdb_path: &Path) -> ProviderResult<RocksDBProvider> {
    RocksDBProvider::builder(rocksdb_path)
        .with_default_tables()
        .with_read_only(true)
        .build()
}
