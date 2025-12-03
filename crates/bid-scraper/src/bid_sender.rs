use std::sync::Arc;

use runng::{protocol::Pub0, SendSocket};
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::types::ScrapedRelayBlockBid;

/// Trait for sending scraped bids.
pub trait BidSender: Send + Sync {
    fn send(&self, bid: ScrapedRelayBlockBid) -> Result<(), BidSenderError>;
}

/// Implementation of BidSender that publishes the bids to the network using NNG.
pub struct NNGBidSender {
    nng_publisher_socket: Pub0,
}

#[derive(Debug, thiserror::Error)]
pub enum BidSenderError {
    #[error("json serialize error")]
    JSON(#[from] serde_json::Error),
    #[error("socket error")]
    Communication(#[from] runng::Error),
}

impl NNGBidSender {
    pub fn new(nng_publisher_socket: Pub0) -> Self {
        Self {
            nng_publisher_socket,
        }
    }
}

impl BidSender for NNGBidSender {
    fn send(&self, bid: ScrapedRelayBlockBid) -> Result<(), BidSenderError> {
        match serde_json::to_vec(&bid) {
            Ok(data) => {
                if let Err(err) = self.nng_publisher_socket.send(&data) {
                    error!(err=?err, "nng_publisher_socket.send failed, global cancelling");
                    return Err(err.into());
                }
            }
            Err(err) => {
                error!(err=?err, "serde_json::to_vec failed, cancelling");
                return Err(err.into());
            }
        }
        Ok(())
    }
}

/// Proxy to a BidSender that cancels tokens on errors.
/// Typically json_cancel will kill a single sub service and communication_cancel will kill the whole service.
pub struct BidSenderCanceller {
    communication_cancel: CancellationToken,
    json_cancel: CancellationToken,
    bid_sender: Arc<dyn BidSender>,
}

impl BidSenderCanceller {
    pub fn new(
        bid_sender: Arc<dyn BidSender>,
        communication_cancel: CancellationToken,
        json_cancel: CancellationToken,
    ) -> Self {
        Self {
            bid_sender,
            communication_cancel,
            json_cancel,
        }
    }
}

impl BidSender for BidSenderCanceller {
    fn send(&self, bid: ScrapedRelayBlockBid) -> Result<(), BidSenderError> {
        let res = self.bid_sender.send(bid);
        if let Err(err) = &res {
            match err {
                BidSenderError::Communication(_) => {
                    self.communication_cancel.cancel();
                }
                BidSenderError::JSON(_) => {
                    self.json_cancel.cancel();
                }
            }
        }
        res
    }
}
