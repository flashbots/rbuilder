use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use tokio::sync::broadcast;
use jsonrpsee::server::{RpcModule, Server};
use jsonrpsee::PendingSubscriptionSink;
use tokio_stream::wrappers::BroadcastStream;
use futures::StreamExt;
use tracing::{info, warn};

use crate::{
    building::builders::{UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory},
    live_builder::payload_events::MevBoostSlotData,
};

use super::{
    bidding::{BiddingService, SlotBidder},
    block_finisher::BlockFinisher,
    relay_submit::BuilderSinkFactory,
};

use jsonrpsee::core::server::SubscriptionMessage;

#[derive(Debug)]
pub struct BlockFinisherFactory {
    bidding_service: Box<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: Box<dyn BuilderSinkFactory>,
    tx: broadcast::Sender<String>,
}

impl BlockFinisherFactory {
    pub async fn new(
        bidding_service: Box<dyn BiddingService>,
        block_sink_factory: Box<dyn BuilderSinkFactory>,
    ) -> eyre::Result<Self> {
        // Create a new JSON-RPC server
        let server = Server::builder()
            .set_message_buffer_capacity(5)
            .build("127.0.0.1:8547")
            .await?;
        
        // Create a broadcast channel
        let (tx, _rx) = broadcast::channel::<String>(16);
        
        let mut module = RpcModule::new(tx.clone());

        // Register a subscription method named "eth_stateDiffSubscription"
        module
            .register_subscription("eth_subscribeStateDiffs", "eth_stateDiffSubscription", "eth_unsubscribeStateDiffs", |_params, pending, ctx| async move {
                let rx = ctx.subscribe();
                let stream = BroadcastStream::new(rx);
                Self::pipe_from_stream(pending, stream).await?;
                Ok(())
            })
            .unwrap();
        
        // Get the server's local address
        let addr = server.local_addr()?;
        // Start the server with the configured module
        let handle = server.start(module);

        // We may use the `ServerHandle` to shut it down or manage it ourselves later
        tokio::spawn(handle.stopped());

        // Log that the subscription server has started
        info!("Block subscription server started on {}", addr);

        Ok(Self {
            bidding_service,
            block_sink_factory,
            tx,
        })
    }

    // Method to handle sending messages from the broadcast stream to subscribers
    async fn pipe_from_stream(
        pending: PendingSubscriptionSink,
        mut stream: BroadcastStream<String>,
    ) -> Result<(), jsonrpsee::core::Error> {
        let sink = match pending.accept().await {
            Ok(sink) => sink,
            Err(e) => {
                warn!("Failed to accept subscription: {:?}", e);
                return Ok(());
            }
        };

        loop {
            tokio::select! {
                _ = sink.closed() => {
                    // connection dropped
                    break Ok(())
                },
                maybe_item = stream.next() => {
                    let item = match maybe_item {
                        Some(Ok(item)) => item,
                        Some(Err(e)) => {
                            warn!("Error in WebSocket stream: {:?}", e);
                            break Ok(());
                        },
                        None => {
                            // stream ended
                            break Ok(())
                        },
                    };
                    let msg = SubscriptionMessage::from_json(&item)?;
                    if sink.send(msg).await.is_err() {
                        break Ok(());
                    }
                }
            }
        }
    }
}

impl UnfinishedBlockBuildingSinkFactory for BlockFinisherFactory {
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink> {
        let slot_bidder: Arc<dyn SlotBidder> = self.bidding_service.create_slot_bidder(
            slot_data.block(),
            slot_data.slot(),
            slot_data.timestamp().unix_timestamp() as u64,
        );
        let finished_block_sink = self.block_sink_factory.create_builder_sink(
            slot_data,
            slot_bidder.clone(),
            cancel.clone(),
        );

        let res = BlockFinisher::new(slot_bidder, Arc::from(finished_block_sink), self.tx.clone());
        Arc::new(res)
    }
}
