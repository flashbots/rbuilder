use crate::live_builder::simulation::SimulatedOrderCommand;
use crate::provider::StateProviderFactory as StateProviderFactory2;
use crate::roothash::{calculate_state_root, run_trie_prefetcher, RootHashConfig, RootHashError};
use crate::telemetry::{inc_provider_bad_reopen_counter, inc_provider_reopen_counter};
use alloy_consensus::Header;
use alloy_eips::{BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber};
use parking_lot::{Mutex, RwLock};
use reth::providers::ExecutionOutcome;
use reth::providers::{BlockHashReader, ChainSpecProvider, ProviderFactory};
use reth_chainspec::ChainInfo;
use reth_db::{Database, DatabaseError};
use reth_errors::{ProviderError, ProviderResult, RethResult};
use reth_node_api::NodeTypesWithDB;
use reth_primitives::SealedHeader;
use reth_provider::{
    providers::{ProviderNodeTypes, StaticFileProvider},
    BlockIdReader, BlockNumReader, DatabaseProvider, DatabaseProviderFactory, DatabaseProviderRO,
    HeaderProvider, StateProviderBox, StateProviderFactory, StaticFileProviderFactory,
};
use revm_primitives::B256;
use std::ops::DerefMut;
use std::{ops::RangeBounds, path::PathBuf, sync::Arc};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// This struct is used as a workaround for https://github.com/paradigmxyz/reth/issues/7836
/// it shares one instance of the provider factory that is recreated when inconsistency is detected.
/// This struct should be used on the level of the whole program and ProviderFactory should be extracted from it
/// into the methods that has a lifetime of a slot (e.g. building particular block).
#[derive(Debug, Clone)]
pub struct ProviderFactoryReopener<N: NodeTypesWithDB> {
    provider_factory: Arc<Mutex<ProviderFactory<N>>>,
    chain_spec: Arc<N::ChainSpec>,
    static_files_path: PathBuf,
    /// Last block the Reopener verified consistency for.
    last_consistent_block: Arc<RwLock<Option<BlockNumber>>>,
    /// Patch to disable checking on test mode. Is ugly but ProviderFactoryReopener should die shortly (5/24/2024).
    testing_mode: bool,
}

impl<N: NodeTypesWithDB + ProviderNodeTypes + Clone> ProviderFactoryReopener<N> {
    pub fn new(
        db: N::DB,
        chain_spec: Arc<N::ChainSpec>,
        static_files_path: PathBuf,
    ) -> RethResult<Self> {
        let provider_factory = ProviderFactory::new(
            db,
            chain_spec.clone(),
            StaticFileProvider::read_only(static_files_path.as_path(), true).unwrap(),
        );

        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            static_files_path,
            testing_mode: false,
            last_consistent_block: Arc::new(RwLock::new(None)),
        })
    }

    pub fn new_from_existing(provider_factory: ProviderFactory<N>) -> RethResult<Self> {
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
    pub fn provider_factory_unchecked(&self) -> ProviderFactory<N> {
        self.provider_factory.lock().clone()
    }

    /// This will check if historical block hashes for the given block is correct and if not it will reopen
    /// provider fatory.
    /// This should be used when consistency is required: e.g. building blocks.
    ///
    /// If the current block number is already known at the time of calling this method, you may pass it to
    /// avoid an additional DB lookup for the latest block number.
    pub fn check_consistency_and_reopen_if_needed(&self) -> eyre::Result<ProviderFactory<N>> {
        let best_block_number = self
            .provider_factory_unchecked()
            .last_block_number()
            .map_err(|err| eyre::eyre!("Error getting best block number: {:?}", err))?;
        let mut provider_factory = self.provider_factory.lock();

        // Don't need to check consistency for the block that was just checked.
        let last_consistent_block = *self.last_consistent_block.read();
        if !self.testing_mode && last_consistent_block != Some(best_block_number) {
            match check_block_hash_reader_health(best_block_number, provider_factory.deref_mut()) {
                Ok(()) => {}
                Err(err) => {
                    debug!(?err, "Provider factory is inconsistent, reopening");
                    inc_provider_reopen_counter();

                    *provider_factory = ProviderFactory::new(
                        provider_factory.db_ref().clone(),
                        self.chain_spec.clone(),
                        StaticFileProvider::read_only(self.static_files_path.as_path(), true)
                            .unwrap(),
                    );
                }
            }

            match check_block_hash_reader_health(best_block_number, provider_factory.deref_mut()) {
                Ok(()) => {}
                Err(err) => {
                    inc_provider_bad_reopen_counter();

                    eyre::bail!(
                        "Provider factory is inconsistent after reopening: {:?}",
                        err
                    );
                }
            }

            *self.last_consistent_block.write() = Some(best_block_number);
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

#[derive(Debug, thiserror::Error)]
pub enum HistoricalBlockError {
    #[error("ProviderError while checking block hashes: {0}")]
    ProviderError(#[from] ProviderError),
    #[error("Missing historical block hash for block {missing_hash_block}, latest block: {latest_block}")]
    MissingHash {
        missing_hash_block: u64,
        latest_block: u64,
    },
}

/// Here we check if we have all the necessary historical block hashes in the database
/// This was added as a debugging method because static_files storage was not working correctly
/// last_block_number is the number of the latest committed block (i.e. if we build block 1001 it should be 1000)
pub fn check_block_hash_reader_health<R: BlockHashReader>(
    last_block_number: u64,
    reader: &R,
) -> Result<(), HistoricalBlockError> {
    // evm must have access to block hashes of 256 of the previous blocks
    let blocks_to_check = last_block_number.min(256);
    for i in 0..blocks_to_check {
        let num = last_block_number - i;
        let hash = reader.block_hash(num)?;
        if hash.is_none() {
            return Err(HistoricalBlockError::MissingHash {
                missing_hash_block: num,
                latest_block: last_block_number,
            });
        }
    }

    Ok(())
}

impl<N: NodeTypesWithDB + ProviderNodeTypes + Clone> StateProviderFactory2
    for ProviderFactoryReopener<N>
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.latest()
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

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.best_block_number()
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.block_hash(number)
    }

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

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.last_block_number()
    }

    fn run_trie_prefetcher(
        &self,
        parent_hash: B256,
        simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        cancel: CancellationToken,
    ) {
        run_trie_prefetcher(
            parent_hash,
            Default::default(),
            self,
            simulated_orders,
            cancel,
        );
    }

    fn calculate_state_root(
        &self,
        parent_hash: B256,
        outcome: &ExecutionOutcome,
        config: RootHashConfig,
    ) -> Result<B256, RootHashError> {
        // TODO: this part is failing
        calculate_state_root(self, parent_hash, outcome, Default::default(), config)
    }
}
