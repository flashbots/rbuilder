//! The code here is copied from rbuilder to avoid dep cycles but should be moved to it's own crate

use eyre::{eyre, Context};
use std::fs::read_to_string;
use std::path::Path;
use tracing_subscriber::EnvFilter;

pub fn load_config_toml_and_env<T: serde::de::DeserializeOwned>(
    path: impl AsRef<Path>,
) -> eyre::Result<T> {
    let data = read_to_string(path.as_ref()).with_context(|| {
        eyre!(
            "Config file read error: {:?}",
            path.as_ref().to_string_lossy()
        )
    })?;
    let config: T = toml::from_str(&data).context("Config file parsing")?;
    Ok(config)
}

#[derive(Debug, Clone)]
pub struct LoggerConfig {
    pub env_filter: String,
    pub log_json: bool,
    pub log_color: bool,
}

pub fn setup_tracing_subscriber(config: LoggerConfig) -> eyre::Result<()> {
    let env = EnvFilter::try_new(&config.env_filter)?;
    if config.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env)
            .try_init()
            .map_err(|err| eyre::format_err!("{}", err))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env)
            .with_ansi(config.log_color)
            .try_init()
            .map_err(|err| eyre::format_err!("{}", err))?;
    }
    Ok(())
}
