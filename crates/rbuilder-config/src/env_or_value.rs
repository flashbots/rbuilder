use serde::{Deserialize, Deserializer};
use serde_with::DeserializeAs;
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

impl<'de, T> DeserializeAs<'de, EnvOrValue<T>> for EnvOrValue<T>
where
    T: FromStr,
    String: Deserialize<'de>,
{
    fn deserialize_as<D>(deserializer: D) -> Result<EnvOrValue<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(EnvOrValue(s, std::marker::PhantomData))
    }
}

/// Helper function to resolve Vec<EnvOrValue<T>> to Vec<T>
pub fn resolve_env_or_values<T: FromStr>(values: &[EnvOrValue<T>]) -> eyre::Result<Vec<T>> {
    values
        .iter()
        .try_fold(Vec::new(), |mut acc, v| -> eyre::Result<Vec<T>> {
            let value = v.value()?;
            if v.0.starts_with(ENV_PREFIX) {
                // If it's an environment variable, split by comma
                let parsed: eyre::Result<Vec<T>> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        T::from_str(s).map_err(|_| eyre::eyre!("Failed to parse value: {}", s))
                    })
                    .collect();
                acc.extend(parsed?);
            } else {
                // If it's not an environment variable, just return the single value
                acc.push(
                    T::from_str(&value)
                        .map_err(|_| eyre::eyre!("Failed to parse value: {}", value))?,
                );
            }
            Ok(acc)
        })
}
