use tonic::{Request, Response, Status};

use crate::bidding_service_wrapper::{
    CreateSlotBidderParams, DestroySlotBidderParams, Empty, LandedBlocksParams, MustWinBlockParams,
};

/// Sync version of BiddingService to simplify things since our bidding is sync.
/// As an extra it adds reload() to forward it to BiddingServiceWithReload::reload
/// New blocks and new bids are not here since they are not transferred with gRPC calls.
pub trait BiddingServiceSync: Send + Sync + 'static {
    /// BiddingService
    #[allow(clippy::result_large_err)]
    fn create_slot_bidder(
        &self,
        request: Request<CreateSlotBidderParams>,
    ) -> Result<Response<Empty>, Status>;
    #[allow(clippy::result_large_err)]
    fn destroy_slot_bidder(
        &self,
        request: Request<DestroySlotBidderParams>,
    ) -> Result<Response<Empty>, Status>;
    #[allow(clippy::result_large_err)]
    fn must_win_block(
        &self,
        request: Request<MustWinBlockParams>,
    ) -> Result<Response<Empty>, Status>;
    #[allow(clippy::result_large_err)]
    fn update_new_landed_blocks_detected(
        &self,
        request: Request<LandedBlocksParams>,
    ) -> Result<Response<Empty>, Status>;
    #[allow(clippy::result_large_err)]
    fn update_failed_reading_new_landed_blocks(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Empty>, Status>;

    fn reload(&self);
}
