use reth::{
    primitives::ChainSpec,
    providers::{BlockHashReader, ChainSpecProvider, ProviderFactory},
};
use reth_db::database::Database;
use reth_interfaces::RethResult;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
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
    /// Patch to disable checking on test mode. Is ugly but ProviderFactoryReopener should die shortly (5/24/2024).
    testing_mode: bool,
}

impl<DB: Database + Clone> ProviderFactoryReopener<DB> {
    pub fn new(db: DB, chain_spec: Arc<ChainSpec>, static_files_path: PathBuf) -> RethResult<Self> {
        let provider_factory =
            ProviderFactory::new(db, chain_spec.clone(), static_files_path.clone())?;

        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            static_files_path,
            testing_mode: false,
        })
    }

    pub fn new_from_existing_for_testing(
        provider_factory: ProviderFactory<DB>,
    ) -> RethResult<Self> {
        let chain_spec = provider_factory.chain_spec();
        let static_files_path = provider_factory.static_file_provider().path().to_path_buf();
        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            static_files_path,
            testing_mode: true,
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
    pub fn check_consistency_and_reopen_if_needed(
        &self,
        current_block_number: u64,
    ) -> eyre::Result<ProviderFactory<DB>> {
        let mut provider_factory = self.provider_factory.lock().unwrap();
        if !self.testing_mode {
            match check_provider_factory_health(current_block_number, &provider_factory) {
                Ok(()) => {}
                Err(err) => {
                    debug!(?err, "Provider factory is inconsistent, reopening");
                    *provider_factory = ProviderFactory::new(
                        provider_factory.db_ref().clone(),
                        self.chain_spec.clone(),
                        self.static_files_path.clone(),
                    )?;
                }
            }

            match check_provider_factory_health(current_block_number, &provider_factory) {
                Ok(()) => {}
                Err(err) => {
                    eyre::bail!(
                        "Provider factory is inconsistent after reopening: {:?}",
                        err
                    );
                }
            }
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
    // evm must have access to block hashed of 256 of the previous blocks
    for i in 1u64..=256 {
        let num = current_block_number - i;
        let hash = provider_factory.block_hash(num)?;
        if hash.is_none() {
            eyre::bail!(
                "Missing historical block hash for block {}, current block: {}",
                current_block_number - i,
                current_block_number
            );
        }

        if num == 0 {
            break;
        }
    }

    Ok(())
}
