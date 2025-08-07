use std::sync::Arc;

use bid_scraper::{bid_scraper_client::ScrapedBidsObs, types::BlockBid};
use derivative::Derivative;
use time::OffsetDateTime;

use crate::{
    live_builder::block_output::bidding::interfaces::BlockBidWithStatsObs,
    telemetry::inc_bids_received,
};

/// BlockBid + extra info needed to measure bis travel times on the bidding service.
#[derive(Derivative, Clone, Debug)]
#[derivative(PartialEq, Eq)]
pub struct BlockBidWithStats {
    pub bid: BlockBid,
    /// Time this strucut was created, just before sending it to the bidding service
    #[derivative(PartialEq = "ignore")]
    creation_time: OffsetDateTime,
}

impl BlockBidWithStats {
    pub fn new(bid: BlockBid) -> Self {
        Self {
            bid,
            creation_time: OffsetDateTime::now_utc(),
        }
    }

    pub fn new_for_deserialization(bid: BlockBid, creation_time: OffsetDateTime) -> Self {
        Self { bid, creation_time }
    }

    pub fn creation_time(&self) -> OffsetDateTime {
        self.creation_time
    }
}

pub struct ScrapedBids2BlockBidWithStatsObs {
    obs: Arc<dyn BlockBidWithStatsObs>,
}

impl ScrapedBids2BlockBidWithStatsObs {
    pub fn new(obs: Arc<dyn BlockBidWithStatsObs>) -> Self {
        Self { obs }
    }
}

impl ScrapedBidsObs for ScrapedBids2BlockBidWithStatsObs {
    fn update_new_bid(&self, bid: BlockBid) {
        inc_bids_received(&bid);
        self.obs.update_new_bid(BlockBidWithStats::new(bid));
    }
}
