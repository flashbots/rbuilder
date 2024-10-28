use crate::telemetry::{inc_provider_bad_reopen_counter, inc_provider_reopen_counter};
use alloy_eips::{BlockNumHash, BlockNumberOrTag};
use reth::providers::{BlockHashReader, ChainSpecProvider, ProviderFactory};
use reth_chainspec::{ChainInfo, ChainSpec};
use reth_db::{database::Database, DatabaseError};
use reth_errors::{ProviderError, ProviderResult, RethResult};
use reth_primitives::{BlockHash, BlockNumber, Header, SealedHeader};
use reth_provider::{
    providers::StaticFileProvider, BlockIdReader, BlockNumReader, DatabaseProviderFactory,
    DatabaseProviderRO, HeaderProvider, StateProviderBox, StateProviderFactory,
    StaticFileProviderFactory,
};
use revm_primitives::{B256, U256};
use std::{
    ops::RangeBounds,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};
use tracing::debug;

/// This struct is used as a workaround for https://github.com/paradigmxyz/reth/issues/7836
/// it shares one instance of the provider factory that is recreated when inconsistency is detected.
/// This struct should be used on the level of the whole program and ProviderFactory should be extracted from it
/// into the methods that has a lifetime of a slot (e.g. building particular block).
#[derive(Debug, Clone)]
pub struct ProviderFactoryReopener<DB> {
    provider_factory: Arc<Mutex<ProviderFactory<DB>>>,
    chain_spec: Arc<ChainSpec>,
    static_files_path: PathBuf,
    /// Last block the Reopener verified consistency for.
    last_consistent_block: Arc<RwLock<Option<BlockNumber>>>,
    /// Patch to disable checking on test mode. Is ugly but ProviderFactoryReopener should die shortly (5/24/2024).
    testing_mode: bool,
}

impl<DB: Database + Clone> ProviderFactoryReopener<DB> {
    pub fn new(db: DB, chain_spec: Arc<ChainSpec>, static_files_path: PathBuf) -> RethResult<Self> {
        let provider_factory = ProviderFactory::new(
            db,
            chain_spec.clone(),
            StaticFileProvider::read_only(static_files_path.as_path()).unwrap(),
        );

        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            static_files_path,
            testing_mode: false,
            last_consistent_block: Arc::new(RwLock::new(None)),
        })
    }

    pub fn new_from_existing(provider_factory: ProviderFactory<DB>) -> RethResult<Self> {
        let chain_spec = provider_factory.chain_spec();
        let static_files_path = provider_factory.static_file_provider().path().to_path_buf();
        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            static_files_path,
            testing_mode: true,
            last_consistent_block: Arc::new(RwLock::new(None)),
        })
    }

    /// This will currently available provider factory without verifying if its correct, it can be used
    /// when consistency is not absolutely required
    pub fn provider_factory_unchecked(&self) -> ProviderFactory<DB> {
        self.provider_factory.lock().unwrap().clone()
    }

    /// This will check if historical block hashes for the given block is correct and if not it will reopen
    /// provider fatory.
    /// This should be used when consistency is required: e.g. building blocks.
    ///
    /// If the current block number is already known at the time of calling this method, you may pass it to
    /// avoid an additional DB lookup for the latest block number.
    pub fn check_consistency_and_reopen_if_needed(&self) -> eyre::Result<ProviderFactory<DB>> {
        let best_block_number = self
            .provider_factory_unchecked()
            .last_block_number()
            .map_err(|err| eyre::eyre!("Error getting best block number: {:?}", err))?;
        let mut provider_factory = self.provider_factory.lock().unwrap();

        // Don't need to check consistency for the block that was just checked.
        let last_consistent_block_guard = self.last_consistent_block.read().unwrap();
        let last_consistent_block = *last_consistent_block_guard;
        // Drop before write might be attempted to avoid deadlock!
        drop(last_consistent_block_guard);
        if !self.testing_mode && last_consistent_block != Some(best_block_number) {
            match check_provider_factory_health(best_block_number, &provider_factory) {
                Ok(()) => {}
                Err(err) => {
                    debug!(?err, "Provider factory is inconsistent, reopening");
                    inc_provider_reopen_counter();

                    *provider_factory = ProviderFactory::new(
                        provider_factory.db_ref().clone(),
                        self.chain_spec.clone(),
                        StaticFileProvider::read_only(self.static_files_path.as_path()).unwrap(),
                    );
                }
            }

            match check_provider_factory_health(best_block_number, &provider_factory) {
                Ok(()) => {}
                Err(err) => {
                    inc_provider_bad_reopen_counter();

                    eyre::bail!(
                        "Provider factory is inconsistent after reopening: {:?}",
                        err
                    );
                }
            }

            let mut last_consistent_block = self.last_consistent_block.write().unwrap();
            *last_consistent_block = Some(best_block_number);
        }
        Ok(provider_factory.clone())
    }
}

/// Really ugly, should refactor with the string bellow or use better errors.
pub fn is_provider_factory_health_error(report: &eyre::Error) -> bool {
    report
        .to_string()
        .contains("Missing historical block hash for block")
}

/// Here we check if we have all the necessary historical block hashes in the database
/// This was added as a debugging method because static_files storage was not working correctly
pub fn check_provider_factory_health<DB: Database>(
    current_block_number: u64,
    provider_factory: &ProviderFactory<DB>,
) -> eyre::Result<()> {
    // evm must have access to block hashes of 256 of the previous blocks
    let blocks_to_check = current_block_number.min(256);
    for i in 1..=blocks_to_check {
        let num = current_block_number - i;
        let hash = provider_factory.block_hash(num)?;
        if hash.is_none() {
            eyre::bail!(
                "Missing historical block hash for block {}, current block: {}",
                num,
                current_block_number
            );
        }
    }

    Ok(())
}

// Implement reth db traits on the ProviderFactoryReopener, allowing generic
// DB access.
//
// ProviderFactory only has access to disk state, therefore cannot implement methods
// that require the blockchain tree (pending state etc.).

impl<DB: Database + Clone> DatabaseProviderFactory<DB> for ProviderFactoryReopener<DB> {
    fn database_provider_ro(&self) -> ProviderResult<DatabaseProviderRO<DB>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.database_provider_ro()
    }
}

impl<DB: Database + Clone> HeaderProvider for ProviderFactoryReopener<DB> {
    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header(block_hash)
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header_by_number(num)
    }

    fn header_td(&self, hash: &BlockHash) -> ProviderResult<Option<U256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header_td(hash)
    }

    fn header_td_by_number(&self, number: BlockNumber) -> ProviderResult<Option<U256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header_td_by_number(number)
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> ProviderResult<Vec<Header>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.headers_range(range)
    }

    fn sealed_header(&self, number: BlockNumber) -> ProviderResult<Option<SealedHeader>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.sealed_header(number)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.sealed_headers_while(range, predicate)
    }
}

impl<DB: Database + Clone> BlockHashReader for ProviderFactoryReopener<DB> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.canonical_hashes_range(start, end)
    }
}

impl<DB: Database + Clone> BlockNumReader for ProviderFactoryReopener<DB> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.best_block_number()
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.last_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.block_number(hash)
    }
}

impl<DB: Database + Clone> BlockIdReader for ProviderFactoryReopener<DB> {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }
}

impl<DB: Database + Clone> StateProviderFactory for ProviderFactoryReopener<DB> {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.latest()
    }

    fn state_by_block_number_or_tag(
        &self,
        _number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.history_by_block_number(block)
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.history_by_block_hash(block)
    }

    fn state_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }

    fn pending_state_by_hash(&self, _block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        unimplemented!("This method is not supported by ProviderFactoryReopener. Please consider using a BlockchainProvider.");
    }
}
