use crate::mev_boost::{
    submission::SubmitBlockRequestWithMetadata, RelayClient, RelayError, SubmitBlockErr,
    ValidatorSlotData,
};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::{Deserialize, Deserializer};
use std::{env, sync::Arc, time::Duration};

/// Usually human readable id for relays. Not used on anything on any protocol just to identify the relays.
pub type MevBoostRelayID = String;

/// Modes for a relay since we may use them for different purposes.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
pub enum RelayMode {
    /// Submits bids, gets slot info. No extra headers on bidding.
    #[serde(rename = "full")]
    #[default]
    Full,
    /// Only gets slot info.
    #[serde(rename = "slot_info")]
    GetSlotInfoOnly,
    /// Submits bids with extra headers. Is not used to get slot info.
    #[serde(rename = "test")]
    Test,
}

impl RelayMode {
    pub fn submits_bids(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => false,
            RelayMode::Test => true,
        }
    }
    pub fn gets_slot_info(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => true,
            RelayMode::Test => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    pub name: String,
    pub url: String,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub authorization_header: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub builder_id_header: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub api_token_header: Option<String>,
    /// mode defines the need of submit_config/priority
    #[serde(default)]
    pub mode: RelayMode,
    #[serde(flatten)]
    /// Submit specific info.
    /// Used only for Full and Fake mode.
    pub submit_config: Option<RelaySubmitConfig>,
    /// priority when getting slot data so solve unconsistencies. Lower number ->  higher priority.
    /// Used only for Full and GetSlotInfoOnly mode.
    pub priority: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct RelaySubmitConfig {
    /// true->ssz false->json
    #[serde(default)]
    pub use_ssz_for_submit: bool,
    #[serde(default)]
    pub use_gzip_for_submit: bool,
    #[serde(default)]
    pub optimistic: bool,
    #[serde(default)]
    pub interval_between_submissions_ms: Option<u64>,
}

impl RelayConfig {
    pub fn with_url(self, url: &str) -> Self {
        Self {
            url: url.to_string(),
            ..self
        }
    }

    pub fn with_name(self, name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..self
        }
    }
}

/// Wrapper in RelayClient to submit blocks.
/// Hides the particular configuration (eg: ssz, gip, optimistic).
#[derive(Debug, Clone)]
pub struct MevBoostRelayBidSubmitter {
    /// Id for UI
    id: MevBoostRelayID,

    client: RelayClient,
    /// true -> ssz; false -> json.
    use_ssz_for_submit: bool,
    use_gzip_for_submit: bool,
    /// Relay accepts optimistic submissions.
    optimistic: bool,
    submission_rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
    /// This is not a real relay so we can send blocks to it even if it does not have any validator registered.
    test_relay: bool,
}

impl MevBoostRelayBidSubmitter {
    pub fn new(
        client: RelayClient,
        id: String,
        config: &RelaySubmitConfig,
        test_relay: bool,
    ) -> Self {
        let submission_rate_limiter = config.interval_between_submissions_ms.map(|d| {
            Arc::new(RateLimiter::direct(
                Quota::with_period(Duration::from_millis(d)).expect("Rate limiter time period"),
            ))
        });
        Self {
            id,
            client,
            use_ssz_for_submit: config.use_ssz_for_submit,
            use_gzip_for_submit: config.use_gzip_for_submit,
            optimistic: config.optimistic,
            submission_rate_limiter,
            test_relay,
        }
    }

    pub fn test_relay(&self) -> bool {
        self.test_relay
    }

    pub fn id(&self) -> &MevBoostRelayID {
        &self.id
    }

    pub fn optimistic(&self) -> bool {
        self.optimistic
    }

    /// false -> rate limiter don't allow
    pub fn can_submit_bid(&self) -> bool {
        if let Some(limiter) = &self.submission_rate_limiter {
            limiter.check().is_ok()
        } else {
            true
        }
    }

    pub async fn submit_block(
        &self,
        data: &SubmitBlockRequestWithMetadata,
    ) -> Result<(), SubmitBlockErr> {
        self.client
            .submit_block(
                data,
                self.use_ssz_for_submit,
                self.use_gzip_for_submit,
                self.test_relay,
            )
            .await
    }
}

/// Wrapper over RelayClient that allows to ask for slot validators info.
#[derive(Debug, Clone)]
pub struct MevBoostRelaySlotInfoProvider {
    /// Id for UI
    id: MevBoostRelayID,
    client: RelayClient,
    /// Lower priority -> more important.
    priority: usize,
}

impl MevBoostRelaySlotInfoProvider {
    pub fn new(client: RelayClient, id: String, priority: usize) -> Self {
        Self {
            client,
            id,
            priority,
        }
    }
    pub fn id(&self) -> &MevBoostRelayID {
        &self.id
    }
    pub fn priority(&self) -> usize {
        self.priority
    }

    /// A little ugly, needed for backtest payload fetcher.
    pub fn client(&self) -> RelayClient {
        self.client.clone()
    }

    pub async fn get_current_epoch_validators(&self) -> Result<Vec<ValidatorSlotData>, RelayError> {
        self.client.get_current_epoch_validators().await
    }
}

fn deserialize_env_var<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(match s {
        Some(val) if val.starts_with("env:") => {
            let env_var = &val[4..];
            env::var(env_var).ok()
        }
        _ => s,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_deserialize_relay_config() {
        let example = "
        name = 'relay1'
        url = 'url'
        priority = 0
        authorization_header = 'env:XXX'
        builder_id_header = 'env:YYY'
        api_token_header = 'env:ZZZ'
        mode = 'slot_info'
        ";

        std::env::set_var("XXX", "AAA");
        std::env::set_var("YYY", "BBB");
        std::env::set_var("ZZZ", "CCC");

        let config: RelayConfig = toml::from_str(example).unwrap();
        assert_eq!(config.name, "relay1");
        assert_eq!(config.url, "url");
        assert_eq!(config.priority, Some(0));
        assert_eq!(config.authorization_header.unwrap(), "AAA");
        assert_eq!(config.builder_id_header.unwrap(), "BBB");
        assert_eq!(config.api_token_header.unwrap(), "CCC");
        assert_eq!(config.mode, RelayMode::GetSlotInfoOnly);
    }

    #[test]
    fn test_deserialize_relay_config_modes() {
        let example_base = "
        name = 'relay1'
        url = 'url'
        mode = "
            .to_string();

        let config: RelayConfig = toml::from_str(&(example_base.clone() + "'full'")).unwrap();
        assert_eq!(config.mode, RelayMode::Full);

        let config: RelayConfig = toml::from_str(&(example_base.clone() + "'slot_info'")).unwrap();
        assert_eq!(config.mode, RelayMode::GetSlotInfoOnly);

        let config: RelayConfig = toml::from_str(&(example_base.clone() + "'test'")).unwrap();
        assert_eq!(config.mode, RelayMode::Test);
    }

    #[test]
    fn test_deserialize_relay_config_no_mode() {
        let config = "
        name = 'relay1'
        url = 'url'";

        let config: RelayConfig = toml::from_str(config).unwrap();
        assert_eq!(config.mode, RelayMode::Full);
    }
}
