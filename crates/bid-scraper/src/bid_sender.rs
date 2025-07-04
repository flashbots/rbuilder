use runng::{protocol::Pub0, SendSocket};
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::types::BlockBid;

/// Struct that publish the bids to the network.
/// signals json_cancel/communication_cancel on errors.
/// Typically json_cancel will kill a single sub service and communication_cancel will kill the whole service.
pub struct BidSender {
    nng_publisher_socket: Pub0,
    communication_cancel: CancellationToken,
    json_cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
pub enum BidSenderError {
    #[error("json serialize error")]
    JSON(#[from] serde_json::Error),
    #[error("socket error")]
    Communication(#[from] runng::Error),
}

impl BidSender {
    pub fn new(
        nng_publisher_socket: Pub0,
        communication_cancel: CancellationToken,
        json_cancel: CancellationToken,
    ) -> Self {
        Self {
            nng_publisher_socket,
            communication_cancel,
            json_cancel,
        }
    }

    pub fn send(&self, bid: BlockBid) -> Result<(), BidSenderError> {
        match serde_json::to_vec(&bid) {
            Ok(data) => {
                if let Err(err) = self.nng_publisher_socket.send(&data) {
                    error!(err=?err, "nng_publisher_socket.send failed, global cancelling");
                    self.communication_cancel.cancel();
                    return Err(err.into());
                }
            }
            Err(err) => {
                error!(err=?err, "serde_json::to_vec failed, cancelling");
                self.json_cancel.cancel();
                return Err(err.into());
            }
        }
        Ok(())
    }
}
