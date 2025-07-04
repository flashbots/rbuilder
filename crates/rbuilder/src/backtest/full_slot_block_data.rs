//! We include here all the info to reproduce everything that happened during the slot.

use std::sync::Arc;

use crate::{
    backtest::{BlockData, BuiltBlockData, OrdersWithTimestamp},
    live_builder::order_input::{
        order_replacement_manager::OrderReplacementManager,
        order_sink::{OrderStore, ShareableOrderSink},
        replaceable_order_sink::ReplaceableOrderSink,
        ReplaceableOrderPoolCommand,
    },
    mev_boost::BuilderBlockReceived,
    utils::offset_datetime_to_timestamp_ms,
};
use ahash::{HashMap, HashSet};
use parking_lot::Mutex;
use time::OffsetDateTime;

/// A ReplaceableOrderPoolCommand + timestamp to be able to reproduce the orderflow timeline.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct ReplaceableOrderPoolCommandWithTimestamp {
    pub timestamp_ms: u64,
    pub command: ReplaceableOrderPoolCommand,
}

impl From<OrdersWithTimestamp> for ReplaceableOrderPoolCommandWithTimestamp {
    fn from(order: OrdersWithTimestamp) -> Self {
        ReplaceableOrderPoolCommandWithTimestamp {
            timestamp_ms: order.timestamp_ms,
            command: ReplaceableOrderPoolCommand::Order(order.order),
        }
    }
}

/// All the information needed to replay a slot.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct FullSlotBlockData {
    pub block_number: u64,
    /// Extra info for landed block (not contained on onchain_block).
    /// We get this from the relays (API /relay/v1/data/bidtraces/builder_blocks_received).
    pub winning_bid_trace: BuilderBlockReceived,
    /// Landed block.
    pub onchain_block: alloy_rpc_types::Block,
    /// Sequence of orders we saw.
    /// To allow exact replay (eg:spam) here there is no filtering (eg: you may have orders with fee below base).
    /// Guarantied to be sorted by increasing timestamp
    available_orders: Vec<ReplaceableOrderPoolCommandWithTimestamp>,
    /// Only available if we landed the block.
    pub built_block_data: Option<BuiltBlockData>,
}

#[derive(Debug, thiserror::Error)]
pub enum FullSlotBlockDataError {
    #[error("Block not won by us")]
    BlockNotWonByUs,
    #[error("Included order not found in fetched order set")]
    IncludedOrderNotFound,
}

impl FullSlotBlockData {
    pub fn new(
        block_number: u64,
        winning_bid_trace: BuilderBlockReceived,
        onchain_block: alloy_rpc_types::Block,
        mut available_orders: Vec<ReplaceableOrderPoolCommandWithTimestamp>,
        built_block_data: Option<BuiltBlockData>,
    ) -> Self {
        available_orders.sort_by_key(|o| o.timestamp_ms);
        Self {
            block_number,
            winning_bid_trace,
            onchain_block,
            available_orders,
            built_block_data,
        }
    }

    pub fn with_available_orders(
        self,
        mut available_orders: Vec<ReplaceableOrderPoolCommandWithTimestamp>,
    ) -> Self {
        available_orders.sort_by_key(|o| o.timestamp_ms);
        Self {
            available_orders,
            ..self
        }
    }

    pub fn available_orders(&self) -> &[ReplaceableOrderPoolCommandWithTimestamp] {
        &self.available_orders
    }

    pub fn built_by_us(&self) -> bool {
        self.built_block_data.is_some()
    }

    /// Creates a snapshot via snapshot and if we landed the block, adds any missing order from the landed orders
    pub fn snapshot_including_landed(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<BlockData, FullSlotBlockDataError> {
        let mut res = self.snapshot(cutoff);
        if let Some(built_block_data) = &self.built_block_data {
            // Gather included orders
            let mut included_orders_ids =
                HashSet::from_iter(built_block_data.included_orders.iter().cloned());
            let included_orders_ids_clone = included_orders_ids.clone();
            let included_orders: Vec<_> = self
                .available_orders
                .iter()
                .flat_map(|command_ts| match &command_ts.command {
                    ReplaceableOrderPoolCommand::Order(order) => {
                        if included_orders_ids.remove(&order.id()) {
                            Some(OrdersWithTimestamp {
                                timestamp_ms: command_ts.timestamp_ms,
                                order: order.clone(),
                            })
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect();
            if !included_orders_ids.is_empty() {
                return Err(FullSlotBlockDataError::IncludedOrderNotFound);
            }
            // Gather replacement_keys
            let included_orders_replacement_keys = HashSet::from_iter(
                included_orders
                    .iter()
                    .flat_map(|o| o.order.replacement_key()),
            );
            // Remove every order with matching id (to avoid dedup later) or replacement key.
            res.available_orders.retain(|o| {
                !included_orders_ids_clone.contains(&o.order.id())
                    && if let Some(rep_key) = o.order.replacement_key() {
                        !included_orders_replacement_keys.contains(&rep_key)
                    } else {
                        true
                    }
            });
            res.available_orders.extend(included_orders);
        }
        Ok(res)
    }

    /// Executes all the commands up to cutoff (included) and generates the orders at that point.
    /// Uses the exact code used in live.
    /// @Pending Set filtered_orders?
    pub fn snapshot(&self, cutoff: OffsetDateTime) -> BlockData {
        let cutoff_ms = offset_datetime_to_timestamp_ms(cutoff);
        let order_store = Arc::new(Mutex::new(OrderStore::new()));
        let mut order_manager =
            OrderReplacementManager::new(Box::new(ShareableOrderSink::new(order_store.clone())));
        // Puaj: patch to regenerate timestamps, I think timestamps should be either removed from BlockData or included in the orders as metadata (if are really needed at some point).
        let mut order_id_to_timestamp = HashMap::default();
        for command_ts in self
            .available_orders
            .clone()
            .into_iter()
            .take_while(|o| o.timestamp_ms <= cutoff_ms)
        {
            match command_ts.command {
                ReplaceableOrderPoolCommand::Order(order) => {
                    order_id_to_timestamp.insert(order.id(), command_ts.timestamp_ms);
                    order_manager.insert_order(order);
                }
                ReplaceableOrderPoolCommand::CancelShareBundle(cancel_share_bundle) => {
                    order_manager.remove_sbundle(cancel_share_bundle.key);
                }
                ReplaceableOrderPoolCommand::CancelBundle(replacement_data) => {
                    order_manager.remove_bundle(replacement_data);
                }
            }
        }

        let available_orders = order_store
            .lock()
            .orders()
            .iter()
            .map(|o| OrdersWithTimestamp {
                timestamp_ms: *order_id_to_timestamp.get(&o.id()).unwrap(),
                order: o.clone(),
            })
            .collect();

        BlockData {
            block_number: self.block_number,
            winning_bid_trace: self.winning_bid_trace.clone(),
            onchain_block: self.onchain_block.clone(),
            available_orders,
            filtered_orders: Default::default(),
            built_block_data: self.built_block_data.clone(),
        }
    }
}
