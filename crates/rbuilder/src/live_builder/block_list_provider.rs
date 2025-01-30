//! Different BlockListProvider flavors.
//! Metrics are updated here, this is ugly.
use ahash::HashSet;
use itertools::Itertools;
use revm_primitives::{Address, B256};
use sha2::{Digest, Sha256};
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

use crate::telemetry::update_blocklist_metrics;

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
pub struct NullBlockListProvider {
    // Make the struct non-constructible from outside by adding a private field
    _private: (),
}

impl NullBlockListProvider {
    pub fn new() -> Self {
        update_blocklist_metrics(&BlockList::default());
        Self { _private: () }
    }
}

impl Default for NullBlockListProvider {
    fn default() -> Self {
        Self::new()
    }
}

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
        update_blocklist_metrics(&list);
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
                            update_blocklist_metrics(&list);
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
                error!("Invalid blocklist");
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
            error!("Invalid blocklist");
            return Err(Error::UnableToLoadInitialList);
        }
        let blocklist: BlockList = blocklist.into_iter().collect();
        update_blocklist_metrics(&blocklist);
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

/// The original code uses text hashing :( (https://github.com/flashbots/ofac/blob/main/hash-blacklist.py)
pub fn blocklist_hash(blocklist: &BlockList) -> B256 {
    let mut hasher = Sha256::new();
    let sorted_text_hashes = blocklist
        .iter()
        .map(|addr| addr.to_string().to_ascii_lowercase())
        .sorted();
    for text in sorted_text_hashes {
        hasher.update(text.as_bytes());
    }
    let hash_bytes = hasher.finalize();
    B256::from_slice(&hash_bytes)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use revm_primitives::{Address, B256};

    use super::{blocklist_hash, BlockList};

    #[test]
    fn test_blocklist_hash() {
        let json = r#"["0x05E0b5B40B7b66098C2161A5EE11C5740A3A7C45","0x01e2919679362dFBC9ee1644Ba9C6da6D6245BB1","0x03893a7c7463AE47D46bc7f091665f1893656003","0x04DBA1194ee10112fE6C3207C0687DEf0e78baCf"]"#;
        let blocklist: Vec<Address> = serde_json::from_str(json).unwrap();
        let blocklist: BlockList = blocklist.into_iter().collect();
        // value generated with https://github.com/flashbots/ofac/blob/main/hash-blacklist.py
        let exected_hash =
            B256::from_str("0xe7d71b5f66f07cdd2a4eaea817e2a133e50f8cc5edea72b53311a7941e6e4b10")
                .unwrap();
        let hash = blocklist_hash(&blocklist);
        assert_eq!(exected_hash, hash);
    }
}
