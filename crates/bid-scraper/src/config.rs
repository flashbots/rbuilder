use serde::Deserialize;
use serde_with::serde_as;

use crate::{
    best_bid_ws_connector::ExternalWsPublisherConfig, bids_publisher::RelayBidsPublisherConfig,
    bloxroute_ws_publisher::BloxrouteWsPublisherConfig,
    headers_publisher::RelayHeadersPublisherConfig,
    ultrasound_ws_publisher::UltrasoundWsPublisherConfig,
};

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PublisherConfig {
    RelayBids(RelayBidsPublisherConfig),
    RelayHeaders(RelayHeadersPublisherConfig),
    UltrasoundWs(UltrasoundWsPublisherConfig),
    BloxrouteWs(BloxrouteWsPublisherConfig),
    ExternalWs(ExternalWsPublisherConfig),
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct NamedPublisherConfig {
    pub name: String,
    #[serde(flatten)]
    pub publisher: PublisherConfig,
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
