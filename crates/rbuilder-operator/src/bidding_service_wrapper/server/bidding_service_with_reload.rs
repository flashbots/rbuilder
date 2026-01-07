use rbuilder::live_builder::block_output::bidding_service_interface::{
    BiddingService, LandedBlockInfo,
};
use rbuilder_primitives::mev_boost::MevBoostRelayID;

use crate::bidding_service_wrapper::server::bidding_service_server_initializer::BiddingServiceFactoryError;

/// Trait that encapsulates the ownership of a BiddingService + the ability to
/// reload the cfg somehow.
pub trait BiddingServiceWithReload: Send + Sync {
    fn bidding_service(&mut self) -> &mut dyn BiddingService;
    fn reload(&mut self);
}

/// Dummy implementation of BiddingServiceWithReload when reload is not available.
pub struct NotReloadingBiddingServiceWithReload {
    bidding_service: Box<dyn BiddingService>,
}

/// Dummy implementation of BiddingServiceWithReload when reload is not available.
impl NotReloadingBiddingServiceWithReload {
    pub fn new(bidding_service: Box<dyn BiddingService>) -> Self {
        Self { bidding_service }
    }
}

impl BiddingServiceWithReload for NotReloadingBiddingServiceWithReload {
    fn bidding_service(&mut self) -> &mut dyn BiddingService {
        self.bidding_service.as_mut()
    }

    fn reload(&mut self) {}
}

pub type BiddingServiceFactory = Box<
    dyn Fn(
            Vec<LandedBlockInfo>,
            &[MevBoostRelayID],
        ) -> Result<Box<dyn BiddingServiceWithReload>, BiddingServiceFactoryError>
        + Send
        + Sync,
>;
