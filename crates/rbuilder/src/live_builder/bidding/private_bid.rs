use alloy_primitives::U256;
use revm_primitives::Address;
use time::OffsetDateTime;
use std::{fmt, hash::{Hash, Hasher}, str::FromStr, sync::Mutex};
use tracing::{info, warn};

use crate::primitives::mev_boost::MevBoostRelay;

use super::{SealInstruction, SlotBidder};
use serde::{Deserialize, Deserializer, Serialize};


#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct BidTrace {
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub slot: U256,
    pub parent_hash: String,
    pub block_hash: String,
    pub builder_pubkey: String,
    pub proposer_pubkey: String,
    pub proposer_fee_recipient: Address,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub gas_limit: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub gas_used: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub value: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub block_number: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub num_tx: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub timestamp: U256,
    #[serde(deserialize_with = "deserialize_u256_from_string")]
    pub timestamp_ms: U256,
}

fn deserialize_u256_from_string<'de, D>(deserializer: D) -> Result<U256, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    U256::from_str(s.as_str()).map_err(serde::de::Error::custom)
}

impl fmt::Display for BidTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BidTrace {{ block_number: {}, builder_pubkey: {}, value: {} , num_tx: {}}}",
            self.block_number, self.builder_pubkey, self.value, self.num_tx
        )
    }
}

impl Hash for BidTrace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
        self.builder_pubkey.hash(state);
    }
}

impl Ord for BidTrace {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl PartialOrd for BidTrace {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}


#[derive(Debug)]
pub struct DynamicOverbidSlotBidder {
    max_overbid_percentage: u64,
    overbid_increment: u64,
    current_overbid_percentage: u64,
    best_bid: Mutex<U256>,
    client: reqwest::Client
}

impl DynamicOverbidSlotBidder {
    pub fn new(
        initial_overbid_percentage: u64,
        max_overbid_percentage: u64,
        overbid_increment: u64,
    ) -> Result<Self, String> {
        if initial_overbid_percentage > max_overbid_percentage {
            return Err("Initial overbid percentage cannot be greater than max".into());
        }
        if overbid_increment == 0 {
            return Err("Overbid increment must be greater than zero".into());
        }

        Ok(Self {
            max_overbid_percentage,
            overbid_increment,
            current_overbid_percentage: initial_overbid_percentage,
            best_bid: Mutex::new(U256::from(0u64)),
            client: reqwest::Client::new(),
        })
    }

    pub async fn get_builder_bids(&self, url: &str, block_num: u64) -> Option<Vec<BidTrace>> {
        let url = format!("{}?block_number={}", url, block_num);
        let res = match self
            .client
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
        {
            Ok(response) =>  {
                response
            }
            Err(e) => {
                eprintln!("Error getting block bids: {}", e);
                return None;
            }
        };

        let bid_traces = match res.json::<Vec<BidTrace>>().await {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Error decoding bids: {}", e);
                return None;
            }
        };
        Some(bid_traces)
    }

    pub async fn update_best_bid(&self, relays: &Vec<MevBoostRelay>, block_number: u64) {
        for relay in relays.iter() {
            let client_url = relay.get_client().url.as_str();
            let url =  format!("{}relay/v1/data/bidtraces/builder_blocks_received", client_url);
            if let Some(bid_traces) = self.get_builder_bids(url.as_str(), block_number).await {
                if !bid_traces.is_empty() {
                    let mut best_bid = self.best_bid.lock().unwrap();
                    if bid_traces[0].value > *best_bid {
                        *best_bid = bid_traces[0].value;
                        info!("Updated best bid to: {}", best_bid);
                    }
                }
            }
        }
    }

    fn calculate_bid(&self, unsealed_block_profit: U256) -> U256 {
        let best_bid = *self.best_bid.lock().unwrap();
        let overbid_amount = best_bid * U256::from(self.current_overbid_percentage) / U256::from(100);
        let our_bid = best_bid + overbid_amount;
        U256::min(our_bid, unsealed_block_profit)
    }
}

impl SlotBidder for DynamicOverbidSlotBidder {
    fn is_pay_to_coinbase_allowed(&self) -> bool {
        true
    }

    fn seal_instruction(&self, unsealed_block_profit: U256, _slot_timestamp: OffsetDateTime) -> SealInstruction {
        let bid = self.calculate_bid(unsealed_block_profit);
        if bid > U256::ZERO && bid <= unsealed_block_profit {
            info!("Sealing with bid: {}", bid);
            SealInstruction::Value(bid)
        } else {
            warn!("Skipping seal due to invalid bid");
            SealInstruction::Skip
        }
    }

    fn best_bid_value(&self, relays: &Vec<MevBoostRelay>, block_number: u64) -> Option<U256> {
        tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async {
            self.update_best_bid(relays, block_number).await;
        });
        Some(*self.best_bid.lock().unwrap())

    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mev_boost_relay_submit_block() {

        let mut relays = Vec::<MevBoostRelay>::new();

        let relay1 = MevBoostRelay::try_from_name_or_url(
            "1",
            format!("https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net").as_str(),
            1, false, false, false, None, None, None, None
        ).unwrap();
        relays.push(relay1);

        let dd = DynamicOverbidSlotBidder::new(10, 100, 10).unwrap();
        let best_value = dd.best_bid_value(&relays, 20491752u64).unwrap();

        println!("best value is {:?}", best_value);

    }

}
