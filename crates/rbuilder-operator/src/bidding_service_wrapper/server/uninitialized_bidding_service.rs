use tonic::{Request, Response, Status};
use tracing::{error, warn};

use crate::bidding_service_wrapper::{
    CreateSlotBidderParams, DestroySlotBidderParams, Empty, LandedBlocksParams, MustWinBlockParams,
};

use super::bidding_service_sync::BiddingServiceSync;

/// Handler for all RPC calls made before calling init.
/// It logs errors for every call.
pub struct UninitializedBiddingService {}

impl UninitializedBiddingService {
    fn status_err() -> Status {
        Status::aborted("initialize not called")
    }
    #[allow(clippy::result_large_err)]
    fn trace_and_error(func_name: &str) -> Result<Response<Empty>, Status> {
        Self::trace(func_name);
        Err(Self::status_err())
    }
    fn trace(func_name: &str) {
        error!(func_name, "Unexpected call before initialize");
    }
}

#[tonic::async_trait]
impl BiddingServiceSync for UninitializedBiddingService {
    /// BiddingService
    fn create_slot_bidder(
        &self,
        _request: Request<CreateSlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        Self::trace("create_slot_bidder");
        Err(Self::status_err())
    }

    fn destroy_slot_bidder(
        &self,
        _request: Request<DestroySlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        Self::trace_and_error("destroy_slot_bidder")
    }

    fn must_win_block(
        &self,
        _request: Request<MustWinBlockParams>,
    ) -> Result<Response<Empty>, Status> {
        Self::trace_and_error("must_win_block")
    }

    fn update_new_landed_blocks_detected(
        &self,
        _request: Request<LandedBlocksParams>,
    ) -> Result<Response<Empty>, tonic::Status> {
        Self::trace_and_error("update_new_landed_blocks_detected")
    }

    fn update_failed_reading_new_landed_blocks(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Empty>, Status> {
        Self::trace_and_error("update_failed_reading_new_landed_blocks")
    }

    fn reload(&self) {
        warn!("reload called with no builder connected");
    }
}
