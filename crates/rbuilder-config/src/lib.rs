use eyre::Context as _;
use serde::de::DeserializeOwned;
use std::{fs, path::Path};

mod logger;
pub use logger::*;

mod env_or_value;
pub use env_or_value::*;

/// Loads configuration from the toml file.
pub fn load_toml_config<T: DeserializeOwned>(path: impl AsRef<Path>) -> eyre::Result<T> {
    let data = fs::read_to_string(path.as_ref()).with_context(|| {
        let path = path.as_ref().to_string_lossy();
        format!("Config file read error: {path:?}",)
    })?;
    let config: T = toml::from_str(&data).context("Config file parsing")?;
    Ok(config)
}
