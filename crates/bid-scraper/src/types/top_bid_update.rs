use crate::{
    get_timestamp_f64,
    types::{PublisherType, ScrapedRelayBlockBid},
};
use alloy_primitives::{Address, BlockHash, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use eyre::eyre;
use ssz::Decode;
use ssz_derive::Decode;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

/// Common to Ultrasound and Titan.
#[derive(Debug, Decode)]
struct TopBidUpdate {
    /// Millisecond timestamp at which this became the top bid
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: BlockHash,
    pub parent_hash: BlockHash,
    pub builder_pubkey: BlsPublicKey,
    pub fee_recipient: Address,
    pub value: U256,
}

fn block_bid_from_update(
    update: TopBidUpdate,
    relay_name: &str,
    publisher_name: &str,
    publisher_type: PublisherType,
) -> ScrapedRelayBlockBid {
    ScrapedRelayBlockBid {
        publisher_name: publisher_name.to_owned(),
        publisher_type: publisher_type.to_owned(),
        builder_pubkey: Some(update.builder_pubkey),
        relay_name: relay_name.to_owned(),
        parent_hash: update.parent_hash,
        block_hash: update.block_hash,
        seen_time: get_timestamp_f64(),
        relay_time: Some(update.timestamp as f64 / 1000.),
        value: update.value,
        slot_number: update.slot,
        gas_used: None,
        fee_recipient: Some(update.fee_recipient),
        proposer_fee_recipient: None,
        optimistic_submission: None,
        block_number: update.block_number,
        extra_data: None,
    }
}

pub fn parse_message(
    message: Message,
    relay_name: &str,
    publisher_name: &str,
    publisher_type: PublisherType,
) -> eyre::Result<Option<ScrapedRelayBlockBid>> {
    match message {
        Message::Binary(data) => {
            let update =
                TopBidUpdate::from_ssz_bytes(&data).map_err(|_| eyre!("unable to deserialize"))?;
            debug!("Got message: {:?}", update);
            let bid = block_bid_from_update(update, relay_name, publisher_name, publisher_type);
            Ok(Some(bid))
        }
        _ => {
            eyre::bail!("Unhandled {publisher_type:?} WS message: {message:?}");
        }
    }
}
