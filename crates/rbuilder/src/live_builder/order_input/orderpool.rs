use ahash::HashMap;
use lru::LruCache;
use rbuilder_primitives::{
    serialize::CancelShareBundle, BundleReplacementData, Order, OrderId, ShareBundleReplacementKey,
};
use reth::providers::StateProviderBox;
use reth_primitives_traits::InMemorySize;
use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self};
use tracing::{error, trace};

use super::{
    order_sink::{OrderPoolCommand, OrderSender2OrderSink},
    replaceable_order_sink::ReplaceableOrderSink,
    ReplaceableOrderPoolCommand,
};

const TIME_TO_KEEP_BUNDLE_CANCELLATIONS: Duration = Duration::from_secs(60);
/// Push to pull for OrderSink. Just poll de UnboundedReceiver to get the orders.
#[derive(Debug)]
pub struct OrdersForBlock {
    pub new_order_sub: mpsc::UnboundedReceiver<OrderPoolCommand>,
}

impl OrdersForBlock {
    /// Helper to create a OrdersForBlock "wrapped" with a OrderSender2OrderSink.
    /// Give this OrdersForBlock to an order pull stage and push on the returned OrderSender2OrderSink
    pub fn new_with_sink() -> (Self, OrderSender2OrderSink) {
        let (sink, sender) = OrderSender2OrderSink::new();
        (
            OrdersForBlock {
                new_order_sub: sender,
            },
            sink,
        )
    }
}

/// Events (orders/cancellations) for a single block
#[derive(Debug, Default)]
struct BundleBlockStore {
    /// Bundles and SharedBundles
    bundles: Vec<Order>,
    cancelled_sbundles: Vec<ShareBundleReplacementKey>,
}

#[derive(Debug)]
struct SinkSubscription {
    sink: Box<dyn ReplaceableOrderSink>,
    block_number: u64,
}

/// returned by add_sink to be used on remove_sink
#[derive(Debug, Eq, Hash, PartialEq, Clone)]
pub struct OrderPoolSubscriptionId(u64);

/// Repository of ALL orders and cancellations that arrives us via process_commands. No processing is done here.
/// The idea is that OrderPool is alive from the start of the universe and we can ask for the
/// events (Orders and cancellations) for a particular block even if the orders arrived in the past.
/// Since by infra restrictions bundle cancellations don't have an associated block so we store them for a while and asume
/// they are valid for all in progress sinks
#[derive(Debug)]
pub struct OrderPool {
    mempool_txs: Vec<(Order, Instant)>,
    /// Sum of measure_tx(order) for all mempool_txs
    mempool_txs_size: usize,
    /// cancelled bundle, cancellation arrival time
    bundle_cancellations: VecDeque<(BundleReplacementData, Instant)>,
    bundles_by_target_block: HashMap<u64, BundleBlockStore>,
    /// Bundles with block == None. Always returned and cleaned on head_updated.
    bundles_for_current_block: Vec<Order>,
    known_orders: LruCache<(OrderId, u64), ()>,
    sinks: HashMap<OrderPoolSubscriptionId, SinkSubscription>,
    next_sink_id: u64,
    /// After this time a mempool tx is dropped.
    time_to_keep_mempool_txs: Duration,
}

impl OrderPool {
    pub fn new(time_to_keep_mempool_txs: Duration) -> Self {
        OrderPool {
            mempool_txs: Vec::new(),
            bundles_by_target_block: HashMap::default(),
            bundles_for_current_block: Default::default(),
            known_orders: LruCache::new(NonZeroUsize::new(10_000).unwrap()),
            sinks: Default::default(),
            next_sink_id: 0,
            bundle_cancellations: Default::default(),
            time_to_keep_mempool_txs,
            mempool_txs_size: 0,
        }
    }

    pub fn process_commands(&mut self, commands: Vec<ReplaceableOrderPoolCommand>) {
        commands.into_iter().for_each(|oc| self.process_command(oc));
    }

    fn process_order(&mut self, order: &Order) {
        let target_block = order.target_block();
        let order_id = order.id();
        if self
            .known_orders
            .contains(&(order_id, target_block.unwrap_or_default()))
        {
            trace!(?order_id, "Order known, dropping");
            return;
        }
        trace!(?order_id, "Adding order");

        let (order, target_block) = match &order {
            Order::Tx(..) => {
                self.mempool_txs.push((order.clone(), Instant::now()));
                self.mempool_txs_size += Self::measure_tx(order);
                (order, None)
            }
            Order::Bundle(bundle) => {
                let target_block = bundle.block;
                match target_block {
                    Some(target_block) => {
                        let bundles_store = self
                            .bundles_by_target_block
                            .entry(target_block)
                            .or_default();
                        bundles_store.bundles.push(order.clone());
                    }
                    None => {
                        self.bundles_for_current_block.push(order.clone());
                    }
                };
                (order, target_block)
            }
            Order::ShareBundle(bundle) => {
                let target_block = bundle.block;
                let bundles_store = self
                    .bundles_by_target_block
                    .entry(target_block)
                    .or_default();
                bundles_store.bundles.push(order.clone());
                (order, Some(target_block))
            }
        };
        self.known_orders
            .put((order.id(), target_block.unwrap_or_default()), ());
    }

    fn process_remove_sbundle(&mut self, cancellation: &CancelShareBundle) {
        let bundles_store = self
            .bundles_by_target_block
            .entry(cancellation.block)
            .or_default();
        bundles_store.cancelled_sbundles.push(cancellation.key);
    }

    fn process_remove_bundle(&mut self, key: &BundleReplacementData) {
        self.bundle_cancellations
            .push_back((key.clone(), Instant::now()));
    }

    fn process_command(&mut self, command: ReplaceableOrderPoolCommand) {
        match &command {
            ReplaceableOrderPoolCommand::Order(order) => self.process_order(order),
            ReplaceableOrderPoolCommand::CancelShareBundle(c) => self.process_remove_sbundle(c),
            ReplaceableOrderPoolCommand::CancelBundle(key) => self.process_remove_bundle(key),
        }
        let target_block = command.target_block();
        self.sinks.retain(|_, sub| {
            if !sub.sink.is_alive() {
                return false;
            }
            if target_block.is_none() || target_block == Some(sub.block_number) {
                let send_ok = match command.clone() {
                    ReplaceableOrderPoolCommand::Order(o) => sub.sink.insert_order(o),
                    ReplaceableOrderPoolCommand::CancelShareBundle(cancel) => {
                        sub.sink.remove_sbundle(cancel.key)
                    }
                    ReplaceableOrderPoolCommand::CancelBundle(replacement_data) => {
                        sub.sink.remove_bundle(replacement_data)
                    }
                };
                if !send_ok {
                    return false;
                }
            }
            true
        });
    }

    /// Adds a sink and pushes the current state for the block
    pub fn add_sink(
        &mut self,
        block_number: u64,
        mut sink: Box<dyn ReplaceableOrderSink>,
    ) -> OrderPoolSubscriptionId {
        for order in self.mempool_txs.iter().map(|(order, _)| order.clone()) {
            sink.insert_order(order);
        }
        for replacement_data in self.bundle_cancellations.iter().map(|(key, _)| key) {
            sink.remove_bundle(replacement_data.clone());
        }

        if let Some(bundle_store) = self.bundles_by_target_block.get(&block_number) {
            for order in bundle_store.bundles.iter().cloned() {
                sink.insert_order(order);
            }
            for key in bundle_store.cancelled_sbundles.iter().cloned() {
                sink.remove_sbundle(key);
            }
        }

        for bundle in self.bundles_for_current_block.iter().cloned() {
            sink.insert_order(bundle);
        }

        let res = OrderPoolSubscriptionId(self.next_sink_id);
        self.next_sink_id += 1;
        self.sinks
            .insert(res.clone(), SinkSubscription { sink, block_number });
        res
    }

    /// Removes the sink. If present returns it
    pub fn remove_sink(
        &mut self,
        id: &OrderPoolSubscriptionId,
    ) -> Option<Box<dyn ReplaceableOrderSink>> {
        self.sinks.remove(id).map(|s| s.sink)
    }

    /// Retains if order is young and nonces are valid.
    pub fn must_retain_order(
        inserted_time: &Instant,
        order: &Order,
        new_state: &StateProviderBox,
        time_to_keep_mempool_txs: &Duration,
    ) -> bool {
        if inserted_time.elapsed() > *time_to_keep_mempool_txs {
            return false;
        }
        for nonce in order.nonces() {
            if nonce.optional {
                continue;
            }
            let onchain_nonce = new_state
                .account_nonce(&nonce.address)
                .map_err(|e: reth_errors::ProviderError| error!("Failed to get a nonce: {}", e))
                .unwrap_or_default()
                .unwrap_or_default();
            if onchain_nonce > nonce.nonce {
                return false;
            }
        }
        true
    }

    /// Should be called when last block is updated.
    /// It's slow but since it only happens at the start of the block it does now matter.
    /// It clears old txs from the mempool and old bundle_cancellations.
    pub fn head_updated(&mut self, new_block_number: u64, new_state: &StateProviderBox) {
        // remove from bundles by target block
        self.bundles_by_target_block
            .retain(|block_number, _| *block_number > new_block_number);
        self.bundles_for_current_block.clear();
        // remove mempool txs by nonce, time
        self.mempool_txs.retain(|(order, time)| {
            let retain =
                Self::must_retain_order(time, order, new_state, &self.time_to_keep_mempool_txs);
            if !retain {
                self.mempool_txs_size -= Self::measure_tx(order);
            }
            retain
        });
        //remove old bundle cancellations
        while let Some((_, oldest_time)) = self.bundle_cancellations.front() {
            if oldest_time.elapsed() < TIME_TO_KEEP_BUNDLE_CANCELLATIONS {
                break; // reached the new ones
            }
            self.bundle_cancellations.pop_front();
        }
    }

    /// Does NOT take in account cancellations
    pub fn content_count(&self) -> (usize, usize) {
        let tx_count = self.mempool_txs.len();
        let bundle_count = self
            .bundles_by_target_block
            .values()
            .map(|v| v.bundles.len())
            .sum();
        (tx_count, bundle_count)
    }

    pub fn mempool_txs_size(&self) -> usize {
        self.mempool_txs_size
    }

    pub fn measure_tx(order: &Order) -> usize {
        match order {
            Order::Tx(tx) => tx.size(),
            Order::Bundle(_) => {
                error!("measure_tx called on a bundle");
                0
            }
            Order::ShareBundle(_) => {
                error!("measure_tx called on an sbundle");
                0
            }
        }
    }
}
