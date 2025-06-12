use serde::Deserialize;
use serde_with::serde_as;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PublisherConfig {
    RelayBids(RelayBidsPublisherConfig),
    RelayHeaders(RelayHeadersPublisherConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedPublisherConfig {
    pub name: String,
    #[serde(flatten)]
    pub publisher: PublisherConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimpleRelayPublisherConfig {
    /// Endpoint for an EL client. Example:"ws://127.0.0.1:8545"
    pub eth_provider_uri: String,

    /// File containing a json list of relays like { "flashbots": "https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net" }
    pub relays_file: String,
    /// Int between [0; --time-offset-count) . We'll initiate our requests at exactly this time proportionally in the slot. Imagine you have 3 instances in 3 servers, you pass --time-offset-count 3 and then the first instance will have --time-offset-index 0, the second 1, and the third 2."
    pub request_interval_s: f64,
    pub time_offset_index: u64,
    pub time_offset_count: u64,
    /// When these jobs should start to query for bids, in each slot. It's then shifted using time_offset_index/time_offset_count.
    /// default_value = "6.0",
    pub request_start_s: f64,
    //#[clap(long, parse(try_from_str = try_parse_custom_request_interval), help="Override the request interval for a specific relay. Use like this: `--custom_request_interval relay_name=0.8`")]
    //pub custom_request_interval_s: Vec<(String, f64)>,
}

pub trait CfgWithSimpleRelayPublisherConfig: Send + Sync {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig;
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayBidsPublisherConfig {
    #[serde(flatten)]
    pub simple_relay_cfg: SimpleRelayPublisherConfig,
}

impl CfgWithSimpleRelayPublisherConfig for RelayBidsPublisherConfig {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig {
        &self.simple_relay_cfg
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayHeadersPublisherConfig {
    /// Endpoint for an EL client. Example:"ws://127.0.0.1:8545"
    pub beacon_node_uri: String,

    #[serde(flatten)]
    pub simple_relay_cfg: SimpleRelayPublisherConfig,
}

impl CfgWithSimpleRelayPublisherConfig for RelayHeadersPublisherConfig {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig {
        &self.simple_relay_cfg
    }
}

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub log_json: bool,
    /// Example: "info"
    pub log_level: String,
    pub log_color: bool,

    /// Where we publish the bids. Example:"tcp://0.0.0.0:5555"
    pub publisher_url: String,

    pub publishers: Vec<NamedPublisherConfig>,
}
