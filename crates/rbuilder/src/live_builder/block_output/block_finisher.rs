use std::sync::Arc;

use tracing::{trace, warn};

use crate::building::builders::{
    block_building_helper::{BlockBuildingHelper, BlockBuildingHelperError},
    UnfinishedBlockBuildingSink,
};

use super::{
    bidding::{SealInstruction, SlotBidder},
    relay_submit::BlockBuildingSink,
};

/// UnfinishedBlockBuildingSink that ask the bidder how much to bid (or skip the block), finishes the blocks and sends them to a BlockBuildingSink.
#[derive(Debug)]
pub struct BlockFinisher {
    /// Bidder we ask how to finish the blocks.
    bidder: Arc<dyn SlotBidder>,
    /// Destination of the finished blocks.
    sink: Arc<dyn BlockBuildingSink>,
}

impl BlockFinisher {
    pub fn new(bidder: Arc<dyn SlotBidder>, sink: Arc<dyn BlockBuildingSink>) -> Self {
        Self { bidder, sink }
    }

    fn finish_and_submit(
        &self,
        block: Box<dyn BlockBuildingHelper>,
    ) -> Result<(), BlockBuildingHelperError> {
        let payout_tx_value = if block.can_add_payout_tx() {
            let available_value = block.true_block_value()?;
            match self
                .bidder
                .seal_instruction(available_value, block.building_context().timestamp())
            {
                SealInstruction::Value(value) => Some(value),
                SealInstruction::Skip => {
                    trace!(
                        block = block.building_context().block(),
                        "Skipped block finalization",
                    );
                    return Ok(());
                }
            }
        } else {
            None
        };
        self.sink
            .new_block(block.finalize_block(payout_tx_value)?.block);
        Ok(())
    }
}

impl UnfinishedBlockBuildingSink for BlockFinisher {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {
        if let Err(err) = self.finish_and_submit(block) {
            warn!(?err, "Error finishing block");
        }
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        self.bidder.is_pay_to_coinbase_allowed()
    }
}
