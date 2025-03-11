use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BidAction {
    SubmitBid { transaction: String },
    CancelBid { bid_id: String },
}
