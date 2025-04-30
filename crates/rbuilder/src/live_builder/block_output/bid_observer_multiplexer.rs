use rbuilder::live_builder::block_output::bid_observer::BidObserver;

/// Implements BidObserver forwarding all calls to several BidObservers.
#[derive(Default)]
pub struct BidObserverMultiplexer {
    observers: Vec<Box<dyn BidObserver + Send + Sync>>,
}

impl BidObserverMultiplexer {
    pub fn push(&mut self, obs: Box<dyn BidObserver + Send + Sync>) {
        self.observers.push(obs);
    }
}

impl BidObserver for BidObserverMultiplexer {
    fn block_submitted(
        &self,
        sealed_block: reth::primitives::SealedBlock,
        submit_block_request: rbuilder::mev_boost::submission::SubmitBlockRequest,
        built_block_trace: rbuilder::building::BuiltBlockTrace,
        builder_name: String,
        best_bid_value: alloy_primitives::U256,
    ) {
        for obs in self.observers {
            obs.block_submitted(
                sealed_block,
                submit_block_request,
                built_block_trace,
                builder_name,
                best_bid_value,
            );
        }
    }
}
