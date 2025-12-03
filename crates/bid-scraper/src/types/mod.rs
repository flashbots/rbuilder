use crate::get_timestamp_f64;

pub mod bid;
pub use bid::{PublisherType, ScrapedRelayBlockBid};

mod bid_update;
pub use bid_update::TopBidUpdate;

pub fn block_bid_from_update(
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
