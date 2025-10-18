pub mod local_data_cache;
pub mod robust_saver;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to load data from local disk")]
    FailedToLoad,

    #[error("Failed to save data to local disk")]
    FailedToSave,

    #[error("File system error")]
    FileSystemError(#[from] std::io::Error),

    #[error("Failed to serialize data")]
    FailedToSerialize,

    #[error("Failed to deserialize data")]
    FailedToDeserialize,
}
