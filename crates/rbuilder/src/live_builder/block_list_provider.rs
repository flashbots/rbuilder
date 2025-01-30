use ahash::HashSet;
use revm_primitives::Address;
use std::{
    fs::read_to_string,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use url::Url;

pub type BlockList = HashSet<Address>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Unable to load initial list")]
    UnableToLoadInitialList,
    #[error("Unable to update list")]
    UnableToUpdateList,
}

const MIN_ITEMS_ON_BLOCK_LIST: usize = 1;

/// Basic validation on a block list.
/// Returns if the list looks good.
fn validate_list(blocklist: &[Address]) -> bool {
    blocklist.len() >= MIN_ITEMS_ON_BLOCK_LIST
}

/// Abstraction to get and update the builder's blocklist.
pub trait BlockListProvider: std::fmt::Debug + Sync + Send {
    /// Gets the a copy of the last list. Fails if it's too old.
    fn get_blocklist(&self) -> Result<BlockList, Error>;
    /// Checks a single address in the current list. Fails if it's too old.
    fn current_list_contains(&self, address: &Address) -> Result<bool, Error>;
}

/// BlockListProvider that always returns Ok(empty list)
#[derive(Debug)]
pub struct NullBlockListProvider {}

impl BlockListProvider for NullBlockListProvider {
    fn get_blocklist(&self) -> Result<BlockList, Error> {
        Ok(BlockList::default())
    }

    fn current_list_contains(&self, _address: &Address) -> Result<bool, Error> {
        Ok(false)
    }
}

#[derive(Debug)]
struct BlockListWithTimestamp {
    pub block_list: BlockList,
    pub timestamp: OffsetDateTime,
}

impl BlockListWithTimestamp {
    fn new(block_list: BlockList) -> Self {
        Self {
            block_list,
            timestamp: OffsetDateTime::now_utc(),
        }
    }
}

const TIMES_TO_UPDATE_PER_MAX_AGE: f32 = 10.0;
/// BlockListProvider that downloads the list from a url and tries to keep it up to date.
/// If it fails to update on time it gives an error.
#[derive(Debug)]
pub struct HttpBlockListProvider {
    max_allowed_age: Duration,
    last_updated_list: Arc<Mutex<BlockListWithTimestamp>>,
}

impl HttpBlockListProvider {
    /// Downloads the file and creates a task to update it periodically.
    pub async fn new(
        url: Url,
        max_allowed_age: Duration,
        validate_list: bool,
        cancellation: CancellationToken,
    ) -> Result<Self, Error> {
        let list = Self::read_list(url.clone(), validate_list)
            .await
            .map_err(|_| Error::UnableToLoadInitialList)?;

        let last_updated_list = Arc::new(Mutex::new(BlockListWithTimestamp::new(list)));
        let last_updated_list_clone = last_updated_list.clone();
        // Spawn a task that continuously reloads the list
        tokio::spawn(async move {
            let period = max_allowed_age.div_f32(TIMES_TO_UPDATE_PER_MAX_AGE);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(period)=> {
                        // mini bug, we ignore the cancellation while downloading the file.
                        if let Ok(list) = Self::read_list(url.clone(),validate_list).await {
                            let list_len = list.len();
                            *last_updated_list.lock().unwrap() = BlockListWithTimestamp::new(list);
                            info!(list_len,"Blocklist updated");
                        }
                    },
                    _ = cancellation.cancelled() =>{
                        return;
                    }
                }
            }
        });

        Ok(Self {
            max_allowed_age,
            last_updated_list: last_updated_list_clone,
        })
    }

    /// lousy error handling since we don't use it much.
    async fn read_list(
        url: Url,
        should_validate_list: bool,
    ) -> Result<BlockList, Box<dyn std::error::Error>> {
        let res = {
            let response = reqwest::get(url.clone()).await?;
            let blocklist = response.bytes().await?;
            let blocklist = String::from_utf8_lossy(&blocklist);
            let blocklist: Vec<Address> = serde_json::from_str(&blocklist)?;
            if !should_validate_list || validate_list(&blocklist) {
                Ok(blocklist.into_iter().collect())
            } else {
                Err("Invalid list".into())
            }
        };
        if let Err(err) = &res {
            error!(err=?err,url=?url,"Error reading blocklist");
        }
        res
    }

    fn lock_current_list(&self) -> Result<MutexGuard<'_, BlockListWithTimestamp>, Error> {
        let last_updated_list = self.last_updated_list.lock().unwrap();
        if OffsetDateTime::now_utc() - last_updated_list.timestamp > self.max_allowed_age {
            return Err(Error::UnableToUpdateList);
        }
        Ok(last_updated_list)
    }
}

impl BlockListProvider for HttpBlockListProvider {
    /// Just gets the last version and checks the age.
    fn get_blocklist(&self) -> Result<BlockList, Error> {
        let last_updated_list = self.lock_current_list()?;
        Ok(last_updated_list.block_list.clone())
    }

    fn current_list_contains(&self, address: &Address) -> Result<bool, Error> {
        let last_updated_list = self.lock_current_list()?;
        Ok(last_updated_list.block_list.contains(address))
    }
}

/// BlockListProvider that opens a file. Useful for backtesting and static scenarios.
/// Can only fail on creation.
/// @Pending upgrade the HttpBlockListProvider to allow to plugin the reader and have a generic updatable source for http/file
#[derive(Debug)]
pub struct StaticFileBlockListProvider {
    block_list: BlockList,
}

impl StaticFileBlockListProvider {
    pub fn new(path: &PathBuf, should_validate_list: bool) -> Result<Self, Error> {
        let blocklist_file = read_to_string(path).map_err(|_| Error::UnableToLoadInitialList)?;
        let blocklist: Vec<Address> =
            serde_json::from_str(&blocklist_file).map_err(|_| Error::UnableToLoadInitialList)?;
        if should_validate_list && !validate_list(&blocklist) {
            return Err(Error::UnableToLoadInitialList);
        }
        Ok(Self {
            block_list: blocklist.into_iter().collect(),
        })
    }
}

impl BlockListProvider for StaticFileBlockListProvider {
    fn get_blocklist(&self) -> Result<BlockList, Error> {
        Ok(self.block_list.clone())
    }

    fn current_list_contains(&self, address: &Address) -> Result<bool, Error> {
        Ok(self.block_list.contains(address))
    }
}
