use futures_util::FutureExt;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        BiddingService, BlockSealInterfaceForSlotBidder, LandedBlockInfo as RealLandedBlockInfo,
        ScrapedRelayBlockBidWithStats, SlotBidder, SlotBlockId,
    },
    utils::build_info::Version,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::error;

use crate::{
    bidding_service_wrapper::{
        bidding_service_client::BiddingServiceClient,
        conversion::{real2rpc_block_hash, real2rpc_landed_block_info},
        fast_streams::helpers::{
            self, create_blocks_publisher, spawn_slot_bidder_seal_bid_command_subscriber,
            BlocksPublisher, ScrapedBidsPublisher,
        },
        CreateSlotBidderParams, DestroySlotBidderParams, Empty, LandedBlocksParams,
        MustWinBlockParams,
    },
    metrics::set_bidding_service_version,
};

use super::unfinished_block_building_sink_client::UnfinishedBlockBuildingSinkClient;

pub struct CreateSlotBidderCommandData {
    params: CreateSlotBidderParams,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
}

#[allow(clippy::large_enum_variant)]
pub enum BiddingServiceClientCommand {
    CreateSlotBidder(CreateSlotBidderCommandData),
    MustWinBlock(MustWinBlockParams),
    UpdateNewLandedBlocksDetected(LandedBlocksParams),
    UpdateFailedReadingNewLandedBlocks,
    DestroySlotBidder(DestroySlotBidderParams),
}

/// Adapts [BiddingServiceClient] to [BiddingService].
/// To adapt sync world ([BiddingService]) to async ([BiddingServiceClient]) it receives commands via a channel (commands_sender)
/// which is handled by a tokio task for everything but heavy duty streams: Blocks, ScrapedBids, and Bids which are handled by iceoryx communication.
/// For every create_slot_bidder call it generates a new session id andcreates a UnfinishedBlockBuildingSinkClient implementing SlotBidder per create_slot_bidder call.
/// The BlockSealInterfaceForSlotBidder passed to create_slot_bidder is stored in a shared map.
/// UnfinishedBlockBuildingSinkClient::notify_new_built_block forwards new blocks to the blocks_publisher (which streams them via iceoryx).
/// BiddingServiceClientAdapter::observe_relay_bids forwards scraped bids to the scraped_bids_publisher (which streams them via iceoryx).
/// A thread is spawned to poll Bids (via iceoryx) from the bidding service and forwards them to the registered BlockSealInterfaceForSlotBidder (shared map filled on create_slot_bidder).
pub struct BiddingServiceClientAdapter {
    commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
    last_session_id: AtomicU64,
    scraped_bids_publisher: ScrapedBidsPublisher,
    blocks_publisher: Arc<BlocksPublisher>,
}

impl std::fmt::Debug for BiddingServiceClientAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BiddingServiceClientAdapter")
            .field("commands_sender", &"<mpsc::UnboundedSender>")
            .field(
                "last_session_id",
                &self
                    .last_session_id
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .field("scraped_bids_publisher", &self.scraped_bids_publisher)
            .finish()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Unable to connect : {0}")]
    TonicTrasport(#[from] tonic::transport::Error),
    #[error("RPC error : {0}")]
    TonicStatus(#[from] tonic::Status),
    #[error("Initialization failed  : {0}")]
    InitFailed(tonic::Status),
    #[error("ScrapedBidsPublisher error : {0}")]
    ScrapedBidsPublisher(#[from] helpers::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

impl BiddingServiceClientAdapter {
    /// @Remove async and reconnect on all create_slot_bidder calls.
    pub async fn new(
        uds_path: &str,
        landed_blocks_history: &[RealLandedBlockInfo],
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let session_id_to_slot_bidder = Arc::new(Mutex::new(HashMap::new()));
        let commands_sender = Self::init_sender_task(
            uds_path,
            landed_blocks_history,
            session_id_to_slot_bidder.clone(),
        )
        .await?;
        spawn_slot_bidder_seal_bid_command_subscriber(
            session_id_to_slot_bidder,
            cancellation_token.clone(),
        )?;
        let scraped_bids_publisher = ScrapedBidsPublisher::new()?;
        let blocks_publisher = Arc::new(create_blocks_publisher(cancellation_token)?);
        Ok(Self {
            commands_sender,
            last_session_id: AtomicU64::new(0),
            scraped_bids_publisher,
            blocks_publisher,
        })
    }

    fn new_session_id(&self) -> u64 {
        self.last_session_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn init_sender_task(
        uds_path: &str,
        landed_blocks_history: &[RealLandedBlockInfo],
        session_id_to_slot_bidder: Arc<
            Mutex<HashMap<u64, Arc<dyn BlockSealInterfaceForSlotBidder + Send + Sync>>>,
        >,
    ) -> Result<mpsc::UnboundedSender<BiddingServiceClientCommand>> {
        let uds_path = uds_path.to_string();
        // Url us dummy but needed to create the Endpoint.
        let channel = Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                // Connect to a Uds socket
                let path = PathBuf::from(uds_path.clone());
                tokio::net::UnixStream::connect(path).map(|result| result.map(TokioIo::new))
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
                        Self::create_slot_bidder(
                            &mut client,
                            create_slot_data,
                            session_id_to_slot_bidder.clone(),
                        )
                        .await;
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
                        session_id_to_slot_bidder
                            .lock()
                            .remove(&destroy_slot_bidder_params.session_id);
                    }
                }
            }
        });
        Ok(commands_sender)
    }

    /// Calls create_slot_bidder via RPC to init the bidder.
    async fn create_slot_bidder(
        client: &mut BiddingServiceClient<Channel>,
        create_slot_bidder_data: CreateSlotBidderCommandData,
        session_id_to_slot_bidder: Arc<
            Mutex<HashMap<u64, Arc<dyn BlockSealInterfaceForSlotBidder + Send + Sync>>>,
        >,
    ) {
        let session_id = create_slot_bidder_data.params.session_id;
        session_id_to_slot_bidder
            .lock()
            .insert(session_id, create_slot_bidder_data.block_seal_handle.into());
        if let Err(err) = client
            .create_slot_bidder(create_slot_bidder_data.params)
            .await
        {
            session_id_to_slot_bidder.lock().remove(&session_id);
            Self::handle_error(Err(err));
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
        _cancel: CancellationToken,
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
                },
            ));
        Arc::new(UnfinishedBlockBuildingSinkClient::new(
            session_id,
            self.commands_sender.clone(),
            self.blocks_publisher.clone(),
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
        self.scraped_bids_publisher.send(bid_with_stats.clone());
    }
}
