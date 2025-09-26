use serde::{Deserialize, Deserializer};
use std::{env::var, str::FromStr};

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
