use alloy_primitives::U256;
use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        BiddingService, BlockId, BlockSealInterfaceForSlotBidder,
        LandedBlockInfo as RealLandedBlockInfo, ScrapedRelayBlockBidWithStats, SlotBidder,
        SlotBidderSealBidCommand, SlotBlockId,
    },
    utils::{build_info::Version, timestamp_us_to_offset_datetime},
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::{error, trace, warn};

use crate::{
    bidding_service_wrapper::{
        bidding_service_client::BiddingServiceClient,
        conversion::{real2rpc_block_bid, real2rpc_block_hash, real2rpc_landed_block_info},
        CreateSlotBidderParams, DestroySlotBidderParams, Empty, LandedBlocksParams,
        MustWinBlockParams, NewBlockParams, UpdateNewBidParams,
    },
    metrics::set_bidding_service_version,
};

use super::unfinished_block_building_sink_client::UnfinishedBlockBuildingSinkClient;

pub struct CreateSlotBidderCommandData {
    params: CreateSlotBidderParams,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
    cancel: tokio_util::sync::CancellationToken,
}

#[allow(clippy::large_enum_variant)]
pub enum BiddingServiceClientCommand {
    CreateSlotBidder(CreateSlotBidderCommandData),
    NewBlock(NewBlockParams),
    UpdateNewBid(UpdateNewBidParams),
    MustWinBlock(MustWinBlockParams),
    UpdateNewLandedBlocksDetected(LandedBlocksParams),
    UpdateFailedReadingNewLandedBlocks,
    DestroySlotBidder(DestroySlotBidderParams),
}

/// Adapts [BiddingServiceClient] to [BiddingService].
/// To adapt sync world ([BiddingService]) to async ([BiddingServiceClient]) it receives commands via a channel (commands_sender)
/// which is handled by a tokio task.
/// It creates a UnfinishedBlockBuildingSinkClient implementing UnfinishedBlockBuildingSink per create_slot_bidder call.
/// For each UnfinishedBlockBuildingSinkClient created a task is created to poll callbacks (eg: bids and can_use_suggested_fee_recipient_as_coinbase updates).
/// The created UnfinishedBlockBuildingSinkClient forwards all calls to the BiddingServiceClientAdapter as commands.
#[derive(Debug)]
pub struct BiddingServiceClientAdapter {
    commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
    last_session_id: AtomicU64,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Unable to connect : {0}")]
    TonicTrasport(#[from] tonic::transport::Error),
    #[error("RPC error : {0}")]
    TonicStatus(#[from] tonic::Status),
    #[error("Initialization failed  : {0}")]
    InitFailed(tonic::Status),
}

pub type Result<T> = core::result::Result<T, Error>;

impl BiddingServiceClientAdapter {
    /// @Remove async and reconnect on all create_slot_bidder calls.
    pub async fn new(
        uds_path: &str,
        landed_blocks_history: &[RealLandedBlockInfo],
    ) -> Result<Self> {
        let commands_sender = Self::init_sender_task(uds_path, landed_blocks_history).await?;
        Ok(Self {
            commands_sender,
            last_session_id: AtomicU64::new(0),
        })
    }

    fn new_session_id(&self) -> u64 {
        self.last_session_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn init_sender_task(
        uds_path: &str,
        landed_blocks_history: &[RealLandedBlockInfo],
    ) -> Result<mpsc::UnboundedSender<BiddingServiceClientCommand>> {
        let uds_path = uds_path.to_string();
        // Url us dummy but needed to create the Endpoint.
        let channel = Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                // Connect to a Uds socket
                let path = PathBuf::from(uds_path.clone());
                tokio::net::UnixStream::connect(path)
            }))
            .await?;
        // Create a client
        let mut client = BiddingServiceClient::new(channel);
        let init_params = LandedBlocksParams {
            landed_block_info: landed_blocks_history
                .iter()
                .map(real2rpc_landed_block_info)
                .collect(),
        };
        let bidding_service_version = client
            .initialize(init_params)
            .await
            .map_err(Error::InitFailed)?;
        let bidding_service_version = bidding_service_version.into_inner();
        set_bidding_service_version(Version {
            git_commit: bidding_service_version.git_commit,
            git_ref: bidding_service_version.git_ref,
            build_time_utc: bidding_service_version.build_time_utc,
        });
        let (commands_sender, mut rx) = mpsc::unbounded_channel::<BiddingServiceClientCommand>();
        // Spawn a task to execute received futures
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    BiddingServiceClientCommand::CreateSlotBidder(create_slot_data) => {
                        Self::create_slot_bidder(&mut client, create_slot_data).await;
                    }
                    BiddingServiceClientCommand::NewBlock(new_block_params) => {
                        Self::handle_error(client.new_block(new_block_params).await);
                    }
                    BiddingServiceClientCommand::UpdateNewBid(update_new_bid_params) => {
                        Self::handle_error(client.update_new_bid(update_new_bid_params).await);
                    }
                    BiddingServiceClientCommand::MustWinBlock(must_win_block_params) => {
                        Self::handle_error(client.must_win_block(must_win_block_params).await);
                    }
                    BiddingServiceClientCommand::UpdateNewLandedBlocksDetected(params) => {
                        Self::handle_error(client.update_new_landed_blocks_detected(params).await);
                    }
                    BiddingServiceClientCommand::UpdateFailedReadingNewLandedBlocks => {
                        Self::handle_error(
                            client
                                .update_failed_reading_new_landed_blocks(Empty {})
                                .await,
                        );
                    }
                    BiddingServiceClientCommand::DestroySlotBidder(destroy_slot_bidder_params) => {
                        Self::handle_error(
                            client.destroy_slot_bidder(destroy_slot_bidder_params).await,
                        );
                    }
                }
            }
        });
        Ok(commands_sender)
    }

    fn parse_option_u256(limbs: Vec<u64>) -> Option<U256> {
        if limbs.is_empty() {
            None
        } else {
            Some(U256::from_limbs_slice(&limbs))
        }
    }

    /// Calls create_slot_bidder via RPC to init the bidder.
    async fn create_slot_bidder(
        client: &mut BiddingServiceClient<Channel>,
        create_slot_bidder_data: CreateSlotBidderCommandData,
    ) {
        match client
            .create_slot_bidder(create_slot_bidder_data.params)
            .await
        {
            Ok(response) => {
                let mut stream = response.into_inner();

                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                                _ = create_slot_bidder_data.cancel.cancelled() => {
                                    return;
                                }
                                callback = stream.next() => {
                                    if let Some(Ok(callback)) = callback {
                                        if let Some(bid) = callback.bid {
                                            let payout_tx_value = Self::parse_option_u256(bid.payout_tx_value);
                                            let seen_competition_bid = Self::parse_option_u256(bid.seen_competition_bid);
                                            let trigger_creation_time = bid.trigger_creation_time_us.map(timestamp_us_to_offset_datetime);
                        let payout_tx_value = if let Some(payout_tx_value) = payout_tx_value {
                        payout_tx_value
                        } else {
                        warn!("payout_tx_value is None");
                        continue;
                        };

                        let seal_command = SlotBidderSealBidCommand {
                        block_id: BlockId(bid.block_id),
                        payout_tx_value,
                        seen_competition_bid,
                        trigger_creation_time,
                        };
                        create_slot_bidder_data.block_seal_handle.seal_bid(seal_command);
                                        } else if let Some(value) = callback.can_use_suggested_fee_recipient_as_coinbase_change {

                        // do nothing as can_use_suggested_fee_recipient_as_coinbase_change is not supported
                        trace!(value, "Got can_use_suggested_fee_recipient_as_coinbase_change from bidding service");
                                        }
                                    }
                                    else {
                                        return;
                                    }
                                }
                            }
                    }
                });
            }
            Err(err) => {
                Self::handle_error(Err(err));
            }
        };
    }

    /// If error logs it.
    /// return result is error
    fn handle_error(result: tonic::Result<tonic::Response<Empty>>) -> bool {
        if let Err(error) = &result {
            error!(error=?error,"RPC call error, killing process so it reconnects");
            std::process::exit(1);
        } else {
            false
        }
    }

    pub async fn must_win_block(&self, block: u64) {
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::MustWinBlock(
                MustWinBlockParams { block },
            ));
    }
}

impl BiddingService for BiddingServiceClientAdapter {
    fn create_slot_bidder(
        &self,
        slot_block_id: SlotBlockId,
        slot_timestamp: OffsetDateTime,
        block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
        cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        // This default will be immediately changed by a callback.
        let session_id = self.new_session_id();
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::CreateSlotBidder(
                CreateSlotBidderCommandData {
                    params: CreateSlotBidderParams {
                        block: slot_block_id.block,
                        slot: slot_block_id.slot,
                        parent_hash: real2rpc_block_hash(slot_block_id.parent_block_hash),
                        session_id,
                        slot_timestamp: slot_timestamp.unix_timestamp(),
                    },
                    block_seal_handle,
                    cancel,
                },
            ));
        Arc::new(UnfinishedBlockBuildingSinkClient::new(
            session_id,
            self.commands_sender.clone(),
        ))
    }

    fn update_new_landed_blocks_detected(&self, landed_blocks: &[RealLandedBlockInfo]) {
        let param = LandedBlocksParams {
            landed_block_info: landed_blocks
                .iter()
                .map(real2rpc_landed_block_info)
                .collect(),
        };
        let _ =
            self.commands_sender
                .send(BiddingServiceClientCommand::UpdateNewLandedBlocksDetected(
                    param,
                ));
    }

    fn update_failed_reading_new_landed_blocks(&self) {
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::UpdateFailedReadingNewLandedBlocks);
    }

    fn observe_relay_bids(&self, bid_with_stats: ScrapedRelayBlockBidWithStats) {
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::UpdateNewBid(
                real2rpc_block_bid(bid_with_stats),
            ));
    }
}
