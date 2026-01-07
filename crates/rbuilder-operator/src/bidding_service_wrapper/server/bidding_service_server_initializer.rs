use crate::bidding_service_wrapper::{
    conversion::{real2rpc_relay_set, rpc2real_relay_set},
    server::bidding_service_with_reload::BiddingServiceFactory,
    BidderVersionInfo, InitParams, InitReturn,
};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::signal::unix::SignalKind;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::{error, info};

use super::{
    bidding_service_server_adapter::BiddingServiceServerAdapter,
    bidding_service_sync::BiddingServiceSync,
    uninitialized_bidding_service::UninitializedBiddingService,
};
use crate::bidding_service_wrapper::{
    bidding_service_server::BiddingService as RPCBiddingService,
    conversion::rpc2real_landed_block_info, CreateSlotBidderParams, DestroySlotBidderParams, Empty,
    LandedBlocksParams, MustWinBlockParams,
};

/// @Pending Consider more errors if they don't leak info.
#[derive(thiserror::Error, Debug)]
pub enum BiddingServiceFactoryError {
    #[error("Unable create BiddingService")]
    UnableToCreateBiddingService,
    #[error("Error opening/parsing config file")]
    UnableToParseConfigFile,
    #[error("Error parsing best competition bid selector cfg")]
    UnableToParseBestCompetitionBidSelectorCfg,
}

/// Handles initialization allowing reconnection.
/// Removes async from the calls.
/// Forwards to another RPCBiddingService.
/// On creation the default RPCBiddingService is UninitializedBiddingService that justs traces errors since any call before initialize is unexpected.
/// On initialize creates a new BiddingServiceServerAdapter via a factory stepping on the previous one.
pub struct BiddingServiceServerInitializer {
    rpc_service: Arc<Mutex<Arc<dyn BiddingServiceSync + Send + Sync>>>,
    /// Option only to allow not having it on testing :(
    factory: Option<BiddingServiceFactory>,
    cancellation_token: CancellationToken,
    version_info: Option<BidderVersionInfo>,
}

impl BiddingServiceServerInitializer {
    pub fn new(
        factory: BiddingServiceFactory,
        cancellation_token: CancellationToken,
        version_info: Option<BidderVersionInfo>,
    ) -> Self {
        let rpc_service: Arc<Mutex<Arc<dyn BiddingServiceSync + Send + Sync>>> =
            Arc::new(Mutex::new(Arc::new(UninitializedBiddingService {})));
        let rpc_service_clone = rpc_service.clone();
        if let Ok(mut hangup) = tokio::signal::unix::signal(SignalKind::hangup()) {
            tokio::spawn(async move {
                info!("SIGHUP handler started");
                loop {
                    hangup.recv().await;
                    rpc_service_clone.lock().reload();
                }
            });
        } else {
            error!("Failed to init SIGHUP handler, reload disabled");
        };
        Self {
            cancellation_token,
            rpc_service,
            factory: Some(factory),
            version_info,
        }
    }

    #[cfg(test)]
    pub fn new_initialized(
        service: Box<
            dyn rbuilder::live_builder::block_output::bidding_service_interface::BiddingService,
        >,
    ) -> Self {
        use crate::bidding_service_wrapper::server::bidding_service_with_reload::NotReloadingBiddingServiceWithReload;

        Self {
            rpc_service: Arc::new(Mutex::new(Arc::new(
                BiddingServiceServerAdapter::new(
                    Box::new(NotReloadingBiddingServiceWithReload::new(service)),
                    CancellationToken::new(),
                )
                .unwrap(),
            ))),
            factory: None,
            cancellation_token: CancellationToken::new(),
            version_info: None,
        }
    }
}

#[tonic::async_trait]
impl RPCBiddingService for BiddingServiceServerInitializer {
    /// Call after connection before calling anything. This will really create the BiddingService on the server side.
    async fn initialize(
        &self,
        request: tonic::Request<InitParams>,
    ) -> Result<tonic::Response<InitReturn>, tonic::Status> {
        let params = request.into_inner();
        let landed_block_info = params
            .landed_block_info
            .iter()
            .map(rpc2real_landed_block_info)
            .collect::<Result<Vec<_>, tonic::Status>>()?;
        let all_relay_ids = rpc2real_relay_set(
            params
                .all_relay_ids
                .as_ref()
                .ok_or(Status::invalid_argument("all_relay_ids is required"))?,
        );
        let mut relay_sets = vec![];
        if let Some(factory) = &self.factory {
            match factory(landed_block_info, all_relay_ids.relays()) {
                Ok(mut new_bidding_service) => {
                    relay_sets = new_bidding_service.bidding_service().relay_sets();
                    let new_adapter = match BiddingServiceServerAdapter::new(
                        new_bidding_service,
                        self.cancellation_token.clone(),
                    ) {
                        Ok(new_adapter) => new_adapter,
                        Err(err) => {
                            return Err(Status::internal(err.to_string()));
                        }
                    };
                    *self.rpc_service.lock() = Arc::new(new_adapter);
                }
                Err(err) => {
                    return Err(Status::invalid_argument(err.to_string()));
                }
            }
        }
        Ok(Response::new(InitReturn {
            bidder_version: self.version_info.clone(),
            relay_sets: relay_sets.iter().map(real2rpc_relay_set).collect(),
        }))
    }

    async fn create_slot_bidder(
        &self,
        request: Request<CreateSlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        self.rpc_service.lock().create_slot_bidder(request)
    }

    async fn destroy_slot_bidder(
        &self,
        request: Request<DestroySlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        self.rpc_service.lock().destroy_slot_bidder(request)
    }

    async fn must_win_block(
        &self,
        request: Request<MustWinBlockParams>,
    ) -> Result<Response<Empty>, Status> {
        self.rpc_service.lock().must_win_block(request)
    }

    async fn update_new_landed_blocks_detected(
        &self,
        request: Request<LandedBlocksParams>,
    ) -> Result<Response<Empty>, Status> {
        self.rpc_service
            .lock()
            .update_new_landed_blocks_detected(request)
    }

    async fn update_failed_reading_new_landed_blocks(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Empty>, Status> {
        self.rpc_service
            .lock()
            .update_failed_reading_new_landed_blocks(request)
    }
}
