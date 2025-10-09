use redis::Commands;
use tracing::error;

use super::best_true_value_pusher::{Backend, BuiltBlockInfo};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Redis error {0}")]
    Redis(#[from] redis::RedisError),
    #[error("Json serialization error {0}")]
    JsonSerialization(#[from] serde_json::Error),
}

/// Backend for BestTrueValuePusher that publish data on a redis channel.
pub struct RedisBackend {
    redis: redis::Client,
    channel_name: String,
}

impl RedisBackend {
    pub fn new(redis: redis::Client, channel_name: String) -> Self {
        Self {
            redis,
            channel_name,
        }
    }
}

impl Backend for RedisBackend {
    type Connection = redis::Connection;
    type BackendError = Error;

    fn connect(&self) -> Result<Self::Connection, Self::BackendError> {
        Ok(self.redis.get_connection()?)
    }

    fn publish(
        &self,
        connection: &mut Self::Connection,
        best_true_value: &BuiltBlockInfo,
    ) -> Result<(), Self::BackendError> {
        let best_true_value = serde_json::to_string(&best_true_value)?;
        Ok(connection.publish(&self.channel_name, &best_true_value)?)
    }
}
