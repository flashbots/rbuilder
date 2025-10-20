use mockall::automock;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::robust_saver::local_data_cache::{
    BinarySerializer, DataId, DataSerializer, LocalDataCache,
};

/// Trait that provides the tools to save to the final storage and update metrics.
#[automock]
pub trait DataSaver<DataType> {
    /// Saves the data to final storage.
    fn save(&mut self, data: &DataType) -> Result<(), Box<dyn std::error::Error>>;
}

/// For metrics and notifications.
#[automock]
pub trait RobustSaverObserver<DataType> {
    /// We get this notification when DataSaver::save for some data seems to be failing repeatedly when other data is being saved successfully.
    /// This is called only the first time this is detected for a given data.
    /// This usually means some serious error like a db schema problem (eg:data not fitting a column).
    fn update_suspicion_failed_save(&self, data: &DataType);
    /// After some failed DataSaver::save the RobustSaver failed to save the data to disk so it's lost forever.
    fn update_failed_save_to_disk(&self, data: &DataType);
    /// After some failed DataSaver::save the RobustSaver dropped the data since the disk was full.
    fn update_dropped_due_to_disk_full(&self, data: &DataType);
    /// Failed trying to read some data from disk so it's lost forever.
    /// We could store these files in some "problematic" folder to be analyzed later.
    fn update_failed_read_from_disk(&self);
    /// Notifies the number of files on disk and the limit. If count reaches limit we start dropping data.
    /// This is not called for every change, only on flush for now.
    fn update_disk_files_count(&self, count: usize, limit: usize);
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
pub struct RobustSaver<
    DataType: Clone,
    RobustSaverObserverType,
    DataSaverType,
    DataSerializerType: DataSerializer<DataType>,
> {
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
    observer: RobustSaverObserverType,
    cancel_token: CancellationToken,
}

impl<DataType: Serialize + DeserializeOwned + Clone, DataSaverType, RobustSaverObserverType>
    RobustSaver<DataType, RobustSaverObserverType, DataSaverType, BinarySerializer<DataType>>
{
    pub fn new_with_binary_serializer(
        saver: DataSaverType,
        observer: RobustSaverObserverType,
        path: PathBuf,
        cache_capacity: usize,
        max_files_to_keep: usize,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            saver,
            observer,
            failed_data_infos: VecDeque::new(),
            failed_data_since_last_flush: Vec::new(),
            last_save_succeeded: false,
            cache: LocalDataCache::new_with_binary_serializer(path, cache_capacity),
            max_files_to_keep,
            cancel_token,
        }
    }
}

impl<
        DataType: Clone,
        DataSaverType: DataSaver<DataType>,
        RobustSaverObserverType: RobustSaverObserver<DataType>,
        DataSerializerType: DataSerializer<DataType>,
    > RobustSaver<DataType, RobustSaverObserverType, DataSaverType, DataSerializerType>
{
    pub fn new(
        saver: DataSaverType,
        observer: RobustSaverObserverType,
        serializer: DataSerializerType,
        path: PathBuf,
        cache_capacity: usize,
        max_files_to_keep: usize,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            saver,
            observer,
            failed_data_infos: VecDeque::new(),
            failed_data_since_last_flush: Vec::new(),
            last_save_succeeded: false,
            cache: LocalDataCache::new(path, cache_capacity, serializer),
            max_files_to_keep,
            cancel_token,
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
                    self.observer.update_failed_save_to_disk(&data.0);
                }
            }
        }
        self.failed_data_since_last_flush.clear();
    }

    /// Reties a single data item removed from the queue.
    /// On fail the data goes back to the queue unless we reached max capacity in which case it is dropped forever.
    fn retry_data(&mut self, data: &DataType, data_info: DataInfo) {
        let last_save_succeeded = self.last_save_succeeded;
        if self.call_save(&data) {
            self.cache.delete(data_info.id);
        } else {
            let fails_after_success = if last_save_succeeded {
                let fails_after_success = data_info.fails_after_success + 1;
                if fails_after_success == BAD_SAVES_THRESHOLD {
                    self.observer.update_suspicion_failed_save(&data);
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
                self.observer.update_dropped_due_to_disk_full(&data);
                self.cache.delete(data_info.id);
            }
        }
    }
    /// Takes pending data (from failed_data_infos) a tries to save them again.
    /// Tries as many data as possible until the timeout is reached.
    fn retry_failed_data_until(&mut self, time_out: Duration) {
        let start_time = Instant::now();
        let mut pending_saves = self.failed_data_infos.len();
        while !self.cancel_token.is_cancelled()
            && start_time.elapsed() < time_out
            && pending_saves > 0
        {
            if let Some(data_info) = self.failed_data_infos.pop_front() {
                match self.cache.load(&data_info.id) {
                    Ok(data) => {
                        self.retry_data(&data, data_info);
                    }
                    Err(err) => {
                        error!(?err, "Failed to load data to disk (dropped forever)");
                        self.cache.delete(data_info.id);
                        self.observer.update_failed_read_from_disk();
                    }
                }
                pending_saves -= 1;
            } else {
                return;
            }
        }
    }

    /// Flushes the failed data to the saver and stores on disk last failed data.
    /// Works at most until the timeout or the cancel token is cancelled.
    /// Notifies update_disk_files_count.
    pub fn flush(&mut self, time_out: Duration) {
        self.save_new_failed_data();
        self.retry_failed_data_until(time_out);
        self.observer
            .update_disk_files_count(self.failed_data_infos.len(), self.max_files_to_keep);
    }
}

mod tests {
    use mockall::predicate::eq;
    use std::io;
    use tempfile::TempDir;

    use super::*;

    const INFINITE_TIME_OUT: Duration = Duration::from_secs(10000);
    const MAX_FILES_TO_KEEP: usize = 5;
    const CACHE_CAPACITY: usize = 3;
    fn save_error() -> Result<(), Box<dyn std::error::Error>> {
        Err(Box::new(io::Error::new(io::ErrorKind::Other, "Failed")))
    }

    #[test]
    /// Test a good save follow that lots of failes causing to drop 1 data.
    fn test_ok_fail_ok() {
        let temp_dir = TempDir::new().unwrap();
        let mut saver = MockDataSaver::<u64>::new();
        let mut observer = MockRobustSaverObserver::<u64>::new();
        const FAIL_COUNT: usize = MAX_FILES_TO_KEEP + 1;

        // Good save
        saver
            .expect_save()
            .with(eq(0))
            .returning(|_| Ok(()))
            .times(1);

        // Fail saves
        for i in 1..=FAIL_COUNT {
            saver
                .expect_save()
                .with(eq(i as u64))
                .returning(|_| save_error())
                .times(1);
        }

        // fail again during first flush
        for i in 1..=FAIL_COUNT {
            saver
                .expect_save()
                .with(eq(i as u64))
                .returning(|_| save_error())
                .times(1);
        }

        //Expect to drop 1 data due to disk full
        observer
            .expect_update_dropped_due_to_disk_full()
            .return_const(())
            .times(1);

        // flush notifies the disk files count
        observer
            .expect_update_disk_files_count()
            .with(eq(MAX_FILES_TO_KEEP), eq(MAX_FILES_TO_KEEP))
            .return_const(())
            .times(1);

        // final good saves on second flush.
        // Notice 1 will not be saved since it was dropped due to disk full.
        for i in 2..=FAIL_COUNT {
            saver
                .expect_save()
                .with(eq(i as u64))
                .returning(|_| Ok(()))
                .times(1);
        }

        // flush notifies the disk files count
        observer
            .expect_update_disk_files_count()
            .with(eq(0), eq(MAX_FILES_TO_KEEP))
            .return_const(())
            .times(1);

        let mut robust_saver = RobustSaver::<
            u64,
            MockRobustSaverObserver<u64>,
            MockDataSaver<u64>,
            BinarySerializer<u64>,
        >::new_with_binary_serializer(
            saver,
            observer,
            temp_dir.path().to_path_buf(),
            CACHE_CAPACITY,
            MAX_FILES_TO_KEEP,
            CancellationToken::new(),
        );

        // First save will succeed
        robust_saver.save(0);
        // All saves will fail
        for i in 1..=FAIL_COUNT {
            robust_saver.save(i as u64);
        }
        // All saves will fail
        robust_saver.flush(INFINITE_TIME_OUT);
        // All saves will succeed
        robust_saver.flush(INFINITE_TIME_OUT);
    }

    #[test]
    /// Test a sequence good/fail 1 G, 2 F, 3 G, 2 F, 5 G, 2 F  so after BAD_SAVES_THRESHOLD it thinks
    /// there is something wrong with 2 and notifies the observer update_suspicion_failed_save.
    fn test_suspicion_failed_save() {
        let temp_dir = TempDir::new().unwrap();
        let mut saver = MockDataSaver::<u64>::new();
        let mut observer = MockRobustSaverObserver::<u64>::new();
        const BATCH_COUNT: usize = 3;

        for i in 0..BATCH_COUNT {
            saver
                .expect_save()
                .with(eq((i * 2 + 1) as u64))
                .returning(|_| Ok(()))
                .times(1);
            saver
                .expect_save()
                .with(eq(2))
                .returning(|_| save_error())
                .times(1);
        }

        // Don't care about disk files count
        observer
            .expect_update_disk_files_count()
            .return_const(())
            .times(..);

        observer
            .expect_update_suspicion_failed_save()
            .with(eq(2))
            .return_const(())
            .times(1);

        let mut robust_saver = RobustSaver::<
            u64,
            MockRobustSaverObserver<u64>,
            MockDataSaver<u64>,
            BinarySerializer<u64>,
        >::new_with_binary_serializer(
            saver,
            observer,
            temp_dir.path().to_path_buf(),
            CACHE_CAPACITY,
            MAX_FILES_TO_KEEP,
            CancellationToken::new(),
        );

        // First save will succeed
        robust_saver.save(1);
        robust_saver.save(2);
        robust_saver.save(3);
        // 2 will be saved again
        robust_saver.flush(INFINITE_TIME_OUT);
        robust_saver.save(5);
        // 2 will be saved again
        robust_saver.flush(INFINITE_TIME_OUT);
    }
}
