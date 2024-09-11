use std::sync::Arc;

use tracing::{trace, info, warn};

use crate::building::builders::{
    block_building_helper::{BlockBuildingHelper, BlockBuildingHelperError},
    UnfinishedBlockBuildingSink,
};

use super::{
    bidding::{SealInstruction, SlotBidder},
    relay_submit::BlockBuildingSink,
};

use tokio::sync::broadcast;
use serde_json::json;
use alloy_primitives::{B256};
use alloy_rpc_types_eth::state::{StateOverride, AccountOverride};
use std::collections::HashMap;
use uuid::Uuid;

/// UnfinishedBlockBuildingSink that ask the bidder how much to bid (or skip the block), finishes the blocks and sends them to a BlockBuildingSink.
#[derive(Debug)]
pub struct BlockFinisher {
    /// Bidder we ask how to finish the blocks.
    bidder: Arc<dyn SlotBidder>,
    /// Destination of the finished blocks.
    sink: Arc<dyn BlockBuildingSink>,
    tx: broadcast::Sender<String>
}

impl BlockFinisher {
    pub fn new(bidder: Arc<dyn SlotBidder>, sink: Arc<dyn BlockBuildingSink>, tx: broadcast::Sender<String>) -> Self {
        Self { bidder, sink, tx }
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
        info!("Block submitted");
        Ok(())
    }
}

impl UnfinishedBlockBuildingSink for BlockFinisher {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {

        let building_context = block.building_context();
        let bundle_state = block.get_bundle_state().state();

        // Create a new StateOverride object to store the changes
        let mut pending_state = StateOverride::new();

        // Iterate through each address and account in the bundle state
        for (address, account) in bundle_state.iter() {
            let mut account_override = AccountOverride::default();
            
            let mut state_diff = HashMap::new();
            for (storage_key, storage_slot) in &account.storage {
                let key = B256::from(*storage_key);
                let value = B256::from(storage_slot.present_value);
                state_diff.insert(key, value);
            }
            
            if !state_diff.is_empty() {
                account_override.state_diff = Some(state_diff);
                pending_state.insert(*address, account_override);
            }
            
        }

        let block_data = json!({
            "blockNumber": building_context.block_env.number,
            "blockTimestamp": building_context.block_env.timestamp,
            "blockUuid": Uuid::new_v4().to_string(),
            "pendingState": pending_state
        });

        if let Err(e) = self.tx.send(serde_json::to_string(&block_data).unwrap()) {
            warn!("Failed to send block data");
        }

        info!(
            "Block generated. Order Count: {}", block.built_block_trace().included_orders.len()
        );

        if let Err(err) = self.finish_and_submit(block) {
            warn!(?err, "Error finishing block");
        }
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        self.bidder.is_pay_to_coinbase_allowed()
    }
}