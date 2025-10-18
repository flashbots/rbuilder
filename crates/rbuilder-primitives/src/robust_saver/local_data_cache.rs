use super::Error;
use fulgurance::{prelude::MruCache, CachePolicy};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    marker::PhantomData,
    path::PathBuf,
};
use tracing::error;
use uuid::Uuid;
/// Trait that provides the tools to serialize and deserialize data.
/// Allows decouple the serialization from the data (eg: it may enable zip compression)
pub trait DataSerializer<DataType> {
    /// This will be used to save the data to disk in case of failure.
    fn serialize_data(&self, data: &DataType) -> Result<Vec<u8>, Error>;
    /// This will be used to read the data from disk in case it's not in memory.
    fn deserialize_data(&self, data: &[u8]) -> Result<DataType, Error>;
}

/// Serializer that uses bincode for binary serialization.
pub struct BinarySerializer<DataType: Serialize + DeserializeOwned> {
    _phantom: PhantomData<DataType>,
}

impl<DataType: Serialize + DeserializeOwned> DataSerializer<DataType>
    for BinarySerializer<DataType>
{
    fn serialize_data(&self, data: &DataType) -> Result<Vec<u8>, Error> {
        Ok(bincode::serialize(data).map_err(|_| Error::FailedToSerialize)?)
    }
    fn deserialize_data(&self, data: &[u8]) -> Result<DataType, Error> {
        Ok(bincode::deserialize(data).map_err(|_| Error::FailedToDeserialize)?)
    }
}

/// Id for the data in the cache and on disk.
pub type DataId = String;

/// Struct that handles Data storage in local memory and disk. Keeps the oldest loaded N elements in memory.
/// We keep oldest since we assume that those are the ones that are most likely to be asked soon.
pub struct LocalDataCache<DataType: Clone, DataSerializerType: DataSerializer<DataType>> {
    /// Where we store the cache files.
    path: PathBuf,
    cache: MruCache<DataId, DataType>,
    serializer: DataSerializerType,
}

impl<DataType: Serialize + DeserializeOwned + Clone>
    LocalDataCache<DataType, BinarySerializer<DataType>>
{
    pub fn new_with_binary_serializer(path: PathBuf, cache_capacity: usize) -> Self {
        Self::new(
            path,
            cache_capacity,
            BinarySerializer::<DataType> {
                _phantom: PhantomData,
            },
        )
    }
}

impl<DataType: Clone, DataSerializerType: DataSerializer<DataType>>
    LocalDataCache<DataType, DataSerializerType>
{
    pub fn new(path: PathBuf, cache_capacity: usize, serializer: DataSerializerType) -> Self {
        let cache = MruCache::new(cache_capacity);
        Self {
            path,
            cache,
            serializer,
        }
    }

    /// Tries to load from memory first and if not found, tries to load from disk.
    /// Failure means disk problems.
    pub fn load(&mut self, id: &DataId) -> Result<DataType, Error> {
        let item = self.cache.get(id);
        if let Some(item) = item {
            Ok(item.clone())
        } else {
            let mut file = File::open(self.data_file_path(&id))?;
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            let data: DataType = self.serializer.deserialize_data(&data)?;
            Ok(data)
        }
    }

    /// Saves to disk and if the cache is not full, it also saves to memory.
    /// Failure means disk problems.
    pub fn save(&mut self, data: DataType) -> Result<DataId, Error> {
        let id = Uuid::new_v4().to_string();
        let binary_data = self.serializer.serialize_data(&data)?;
        let mut file = File::create(self.data_file_path(&id))?;
        file.write_all(&binary_data)?;
        if self.cache.len() < self.cache.capacity() {
            self.cache.insert(id.clone(), data);
        }
        Ok(id)
    }

    /// Deletes the data from the cache and disk.
    /// Does not return an error (nothing we can do about it), just logs the error.
    pub fn delete(&mut self, id: DataId) {
        self.cache.remove(&id);
        if let Err(err) = std::fs::remove_file(self.data_file_path(&id)) {
            error!(?err, ?id, "Failed to delete data from disk");
        }
    }

    fn data_file_path(&self, id: &DataId) -> PathBuf {
        self.path.join(id)
    }
}

#[cfg(test)]
mod test {
    use ahash::HashMap;
    use tempfile::TempDir;

    fn file_count(dir: PathBuf) -> usize {
        let entries = std::fs::read_dir(dir).unwrap();
        entries.count()
    }

    use super::*;
    #[test]
    fn test_local_data_cache() {
        let temp_dir = TempDir::new().unwrap();
        const CACHE_CAPACITY: usize = 4;
        let mut cache = LocalDataCache::<u64, _>::new_with_binary_serializer(
            temp_dir.path().to_path_buf(),
            CACHE_CAPACITY,
        );

        // Until reaching the cache capacity, we should have the same number of files as the number of elements saved.
        let mut ids = HashMap::default();
        for i in 0..CACHE_CAPACITY * 2 {
            let id = cache.save(i as u64).unwrap();
            ids.insert(id, i as u64);
            assert_eq!(file_count(temp_dir.path().to_path_buf()), i + 1);
        }
        for (k, v) in ids.iter() {
            let data = cache.load(&k).unwrap();
            assert_eq!(data, *v);
        }
    }
}
