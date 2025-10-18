use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing::error;

use crate::robust_saver::local_data_cache::{
    BinarySerializer, DataId, DataSerializer, LocalDataCache,
};

/// Trait that provides the tools to save to the final storage and update metrics.
pub trait DataSaver<DataType> {
    /// Saves the data to final storage.
    fn save(&mut self, data: &DataType) -> Result<(), Box<dyn std::error::Error>>;
    /// We get this notification when save for some data seems to be failing repeatedly when other data is being saved successfully.
    /// This is called only the first time this is detected for a given data.
    /// This usually means some serious error like a db schema problem (eg:data not fitting a column).
    fn notify_suspicion_failed_save(&mut self, data: &DataType);
}

struct DataInfo {
    id: DataId,
    /// Counts how many times the data failed to save after a successful save which could be caused
    /// by a more severe error than just a temporary error.
    fails_after_success: usize,
}

/// After if reach this number "bad saves" (save failures after a successful save) we consider save might have a persistent problem.
const BAD_SAVES_THRESHOLD: usize = 3;

/// Struct that allows saving data with reties using local storage if the pending data is too much to keep in memory.
/// "Save" is used as a generic term to describe the action of saving data to any persistent storage like clickhouse, SQL, etc.
/// usage:
/// - Call save() to save data to the saver. No disk operations are performed here.
/// - Call flush() to flush the failed data to the saver and store on disk last failed data.
///   Flush is not automatically called to allow the owner to control the frequency and moment (beginning of slot is usually a good moment) of the flush operations.
pub struct RobustSaver<DataType: Clone, DataSaverType, DataSerializerType: DataSerializer<DataType>>
{
    saver: DataSaverType,
    /// Data that failed to save to the saver and was not inserted on the cache (no file on disk) to avoid hitting the disk too much.
    /// The bool indicates if the previous save succeeded (we need it to set initial value for DataInfo: fails_after_success)
    failed_data_since_last_flush: Vec<(DataType, bool)>,
    /// Ids of the data that failed to save to the saver and was inserted on the cache (has a file on disk).
    failed_data_infos: VecDeque<DataInfo>,
    last_save_succeeded: bool,
    cache: LocalDataCache<DataType, DataSerializerType>,
    /// This limits the number of files on disk to avoid filling the disk with failed data.
    max_files_to_keep: usize,
}

impl<DataType: Serialize + DeserializeOwned + Clone, DataSaverType>
    RobustSaver<DataType, DataSaverType, BinarySerializer<DataType>>
{
    pub fn new_with_binary_serializer(
        saver: DataSaverType,
        path: PathBuf,
        cache_capacity: usize,
        max_files_to_keep: usize,
    ) -> Self {
        Self {
            saver,
            failed_data_infos: VecDeque::new(),
            failed_data_since_last_flush: Vec::new(),
            last_save_succeeded: false,
            cache: LocalDataCache::new_with_binary_serializer(path, cache_capacity),
            max_files_to_keep,
        }
    }
}

impl<
        DataType: Clone,
        DataSaverType: DataSaver<DataType>,
        DataSerializerType: DataSerializer<DataType>,
    > RobustSaver<DataType, DataSaverType, DataSerializerType>
{
    pub fn new(
        saver: DataSaverType,
        serializer: DataSerializerType,
        path: PathBuf,
        cache_capacity: usize,
        max_files_to_keep: usize,
    ) -> Self {
        Self {
            saver,
            failed_data_infos: VecDeque::new(),
            failed_data_since_last_flush: Vec::new(),
            last_save_succeeded: false,
            cache: LocalDataCache::new(path, cache_capacity, serializer),
            max_files_to_keep,
        }
    }

    /// self.saver.save + last_save_succeeded update.
    fn call_save(&mut self, data: &DataType) -> bool {
        match self.saver.save(&data) {
            Ok(()) => {
                self.last_save_succeeded = true;
            }
            Err(err) => {
                error!(?err, "Failed to save data");
                self.last_save_succeeded = false;
            }
        }
        self.last_save_succeeded
    }

    /// Saves the data to the saver. If the save fails, the data is stored in the failed data buffer.
    /// No disk access is performed here.
    pub fn save(&mut self, data: DataType) {
        let last_save_succeeded = self.last_save_succeeded;
        if !self.call_save(&data) {
            self.failed_data_since_last_flush
                .push((data, last_save_succeeded));
        }
    }

    // Save data from failed save calls since last call.
    fn save_new_failed_data(&mut self) {
        for data in &self.failed_data_since_last_flush {
            match self.cache.save(data.0.clone()) {
                Ok(id) => {
                    self.failed_data_infos.push_back(DataInfo {
                        id,
                        fails_after_success: if data.1 { 1 } else { 0 },
                    });
                }
                Err(err) => {
                    error!(?err, "Failed to save data to disk (dropped forever)");
                    // @Pending: Metric data loss.
                }
            }
        }
    }

    /// Reties a single data item.
    /// On fail the data goes back to the queue unless we reached max capacity in which case it is dropped forever.
    fn retry_data(&mut self, data: &DataType, data_info: DataInfo) {
        let last_save_succeeded = self.last_save_succeeded;
        if self.call_save(&data) {
            self.cache.delete(data_info.id);
        } else {
            let fails_after_success = if last_save_succeeded {
                let fails_after_success = data_info.fails_after_success + 1;
                if fails_after_success == BAD_SAVES_THRESHOLD {
                    // @Pending: Metric bad save.
                }
                fails_after_success
            } else {
                data_info.fails_after_success
            };
            if self.failed_data_infos.len() < self.max_files_to_keep {
                // Back to the queue for later retry.
                self.failed_data_infos.push_back(DataInfo {
                    id: data_info.id,
                    fails_after_success,
                });
            } else {
                // Drop the data from the cache and disk, lost for ever.
                // @Pending: Metric data loss.
                self.cache.delete(data_info.id);
            }
        }
    }
    /// Takes pending data (from failed_data_infos) a tries to save them again.
    /// Tries as many data as possible until the timeout is reached.
    fn retry_failed_data_until(&mut self, time_out: Duration) {
        let start_time = Instant::now();
        while start_time.elapsed() < time_out {
            if let Some(data_info) = self.failed_data_infos.pop_front() {
                match self.cache.load(&data_info.id) {
                    Ok(data) => {
                        self.retry_data(&data, data_info);
                    }
                    Err(err) => {
                        error!(?err, "Failed to load data to disk (dropped forever)");
                        self.cache.delete(data_info.id);
                        // @Pending: Metric data loss.
                    }
                }
            }
        }
    }

    /// Flushes the failed data to the saver and stores on disk last failed data.
    /// Works at most until the timeout or the cancel token is cancelled.
    pub fn flush(
        &mut self,
        time_out: Duration,
        //cancel_token: CancellationToken,
    ) {
        self.save_new_failed_data();
        self.retry_failed_data_until(time_out);
    }
}
