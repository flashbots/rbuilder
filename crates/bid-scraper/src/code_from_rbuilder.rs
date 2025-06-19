//! The code here is copied from rbuilder to avoid dep cycles but should be moved to it's own crate.

use eyre::{eyre, Context};
use serde::{Deserialize, Deserializer};
use std::env::var;
use std::fs::read_to_string;
use std::path::Path;
use std::str::FromStr;
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

/// Prefix for env variables in config
const ENV_PREFIX: &str = "env:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOrValue<T>(String, std::marker::PhantomData<T>);

impl<T: FromStr> EnvOrValue<T> {
    pub fn value(&self) -> eyre::Result<String> {
        let value = &self.0;
        if value.starts_with(ENV_PREFIX) {
            let var_name = value.trim_start_matches(ENV_PREFIX);
            var(var_name).map_err(|_| eyre::eyre!("Env variable: {} not set", var_name))
        } else {
            Ok(value.to_string())
        }
    }
}

impl<T> From<&str> for EnvOrValue<T> {
    fn from(s: &str) -> Self {
        Self(s.to_string(), std::marker::PhantomData)
    }
}

impl<'de, T: FromStr> Deserialize<'de> for EnvOrValue<T> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self(s, std::marker::PhantomData))
    }
}
