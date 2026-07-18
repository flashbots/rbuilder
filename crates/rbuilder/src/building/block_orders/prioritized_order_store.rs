use std::{collections::hash_map::Entry, sync::Arc};

use ahash::{HashMap, HashSet};
use alloy_primitives::Address;
use priority_queue::PriorityQueue;

use crate::telemetry::mark_order_not_ready_for_immediate_inclusion;
use rbuilder_primitives::{
    evm_inspector::SlotKey, order_statistics::OrderStatistics, AccountNonce, Nonce, OrderId,
    SimulatedOrder,
};

use super::{OrderPriority, SimulatedOrderSink};

/// Block store that checks the nonces and priorities of the orders so we can easily get the best by calling pop_order()
/// Not orders are ready to be executed due to nonce dependencies.
/// Order must implement BlockOrdersOrder which has priority(). This priority is used to sort the simulated orders.
/// Usage:
/// - Add new order (a little bit complex):
///   ALWAYS BE SURE THAT YOU CALLED update_onchain_nonces and updated the current state of all the needed nonces by the order
///   call insert_order
/// - Get best order to execute
///   call pop_order to get the best order
///   if the order is executed call update_onchain_nonces to update all the changed nonces.
/// - Remove orders: remove_orders. This is useful if we think this orders are no really good (failed to execute to often)
#[derive(Debug, Clone)]
pub struct PrioritizedOrderStore<OrderPriorityType> {
    /// Ready (all nonce matching (or not matched but optional)) to execute orders sorted
    main_queue: PriorityQueue<OrderId, OrderPriorityType>,
    /// For each account we store all the orders from main_queue which contain a tx from this account.
    /// Since the orders belong to main_queue these are orders ready to execute.
    /// As soon as we execute an order from main_queue all orders for all the accounts the order used (order.nonces()) could get invalidated (if tx is not optional).
    main_queue_nonces: HashMap<Address, Vec<OrderId>>,

    /// Up to date "onchain" nonces for the current block we are building.
    /// Special care must be taken to keep this in sync.
    onchain_nonces: HashMap<Address, u64>,

    /// Orders waiting for an account to reach a particular nonce.
    pending_orders: HashMap<AccountNonce, Vec<OrderId>>,
    /// Storage slots written by orders already added to the block this round (monotonic).
    /// Used to release bundles that asked to be attempted after one of their target slots is written.
    written_slots: HashSet<SlotKey>,
    /// Bundles waiting for one of their target storage slots to be written (blind backrunning).
    /// A bundle is listed under every slot it targets and released as soon as any one is written.
    pending_on_slot: HashMap<SlotKey, Vec<OrderId>>,
    /// Id -> order for all orders we manage. Carefully maintained by remove/insert
    orders: HashMap<OrderId, Arc<SimulatedOrder>>,
    /// Everything in orders
    orders_statistics: OrderStatistics,
}

impl<OrderPriorityType: OrderPriority> PrioritizedOrderStore<OrderPriorityType> {
    pub fn new(initial_onchain_nonces: impl IntoIterator<Item = AccountNonce>) -> Self {
        let mut onchain_nonces = HashMap::default();
        for onchain_nonce in initial_onchain_nonces {
            onchain_nonces.insert(onchain_nonce.account, onchain_nonce.nonce);
        }
        Self {
            main_queue: PriorityQueue::new(),
            main_queue_nonces: HashMap::default(),
            onchain_nonces,
            pending_orders: HashMap::default(),
            written_slots: HashSet::default(),
            pending_on_slot: HashMap::default(),
            orders: HashMap::default(),
            orders_statistics: Default::default(),
        }
    }

    pub fn orders_statistics(&self) -> OrderStatistics {
        self.orders_statistics.clone()
    }

    pub fn pop_order(&mut self) -> Option<Arc<SimulatedOrder>> {
        let (id, _) = self.main_queue.pop()?;

        let order = self
            .remove_poped_order(&id)
            .expect("order from prio queue not found in block orders");
        Some(order)
    }

    /// Clean up after some order was removed from main_queue
    fn remove_poped_order(&mut self, id: &OrderId) -> Option<Arc<SimulatedOrder>> {
        let sim_order = self.remove_from_orders(id)?;
        for Nonce { address, .. } in sim_order.order.nonces() {
            match self.main_queue_nonces.entry(address) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().retain(|id| *id != sim_order.id());
                }
                Entry::Vacant(_) => {}
            }
        }
        Some(sim_order)
    }

    // if order updates onchain nonce from n -> n + 2, we get n + 2 as an arguments here
    pub fn update_onchain_nonces(&mut self, new_nonces: &[AccountNonce]) {
        let mut invalidated_orders = HashSet::default();
        for new_nonce in new_nonces {
            self.onchain_nonces
                .insert(new_nonce.account, new_nonce.nonce);

            let orders = if let Some(orders) = self.main_queue_nonces.remove(&new_nonce.account) {
                orders
            } else {
                continue;
            };
            for order_id in orders {
                invalidated_orders.insert(order_id);
            }
        }

        for order_id in invalidated_orders {
            // check if order can still be valid because of optional nonces
            self.main_queue.remove(&order_id);
            let order = self
                .remove_poped_order(&order_id)
                .expect("order from prio queue not found in block orders");
            let mut valid = true;
            let mut valid_nonces = 0;
            for Nonce {
                nonce,
                address,
                optional,
            } in order.nonces()
            {
                let onchain_nonce = self
                    .onchain_nonces
                    .get(&address)
                    .cloned()
                    .unwrap_or_default();
                if onchain_nonce > nonce && !optional {
                    valid = false;
                    break;
                } else if onchain_nonce == nonce {
                    valid_nonces += 1;
                }
            }
            let retain_order = valid && valid_nonces > 0;
            tracing::trace!(order = ?order_id, retain_order, "invalidated order");
            if retain_order {
                self.insert_order(order.clone());
            } else {
                mark_order_not_ready_for_immediate_inclusion(&order_id);
            }
        }

        for new_nonce in new_nonces {
            if let Some(pending) = self.pending_orders.remove(new_nonce) {
                let orders = pending
                    .iter()
                    .filter_map(|id| self.remove_from_orders(id))
                    .collect::<Vec<_>>();
                for order in orders {
                    self.insert_order(order);
                }
            }
        }
    }

    /// A bundle that targets storage slots is only ready once at least one of those slots has been
    /// written this block. Orders without target slots are always slot-ready.
    fn order_slots_available(&self, sim_order: &SimulatedOrder) -> bool {
        let target = sim_order.order.target_storage_slots();
        target.is_empty() || target.iter().any(|slot| self.written_slots.contains(slot))
    }

    /// Records the storage slots written by a freshly added order and releases any bundle that asked
    /// to be attempted after one of its target slots was written (blind backrunning, issue #27).
    /// Call this right after an order commits, with the slots that order wrote.
    pub fn notify_slots_written(&mut self, written: &[SlotKey]) {
        // Accumulate first so that released orders see the new writes as available.
        let freshly_written = written
            .iter()
            .filter(|&slot| self.written_slots.insert(slot.clone()))
            .cloned()
            .collect::<Vec<_>>();
        // Every unique bundle waiting on any of the newly written slots.
        let released = freshly_written
            .iter()
            .filter_map(|slot| self.pending_on_slot.remove(slot))
            .flatten()
            .collect::<HashSet<OrderId>>();
        let ready = released
            .into_iter()
            .filter_map(|id| {
                let order = self.remove_from_orders(&id)?;
                // OR-semantics: a bundle listed under several target slots is released by the first
                // one written, so drop it from the buckets of its other (still unwritten) slots.
                order.order.target_storage_slots().iter().for_each(|slot| {
                    if let Some(bucket) = self.pending_on_slot.get_mut(slot) {
                        bucket.retain(|other| *other != id);
                    }
                });
                Some(order)
            })
            .collect::<Vec<_>>();
        ready.into_iter().for_each(|order| self.insert_order(order));
    }

    pub fn get_all_orders(&self) -> Vec<Arc<SimulatedOrder>> {
        self.orders.values().cloned().collect()
    }

    /// Removes from self.orders and updates statistics
    fn remove_from_orders(&mut self, id: &OrderId) -> Option<Arc<SimulatedOrder>> {
        let res = self.orders.remove(id);
        if let Some(sim_order) = &res {
            self.orders_statistics.remove(&sim_order.order);
        }
        res
    }
}

impl<OrderPriorityType: OrderPriority> SimulatedOrderSink
    for PrioritizedOrderStore<OrderPriorityType>
{
    fn insert_order(&mut self, sim_order: Arc<SimulatedOrder>) {
        if self.orders.contains_key(&sim_order.id()) {
            return;
        }
        let mut pending_nonces = Vec::new();
        for Nonce {
            nonce,
            address,
            optional,
        } in sim_order.nonces()
        {
            let onchain_nonce = self
                .onchain_nonces
                .get(&address)
                .cloned()
                .unwrap_or_default();
            if onchain_nonce > nonce && !optional {
                // order can't be included because of nonce
                return;
            }
            if onchain_nonce < nonce && !optional {
                pending_nonces.push(AccountNonce {
                    account: address,
                    nonce,
                });
            }
        }
        let slots_ready = self.order_slots_available(&sim_order);
        if pending_nonces.is_empty() && slots_ready {
            self.main_queue
                .push(sim_order.id(), OrderPriorityType::new(sim_order.clone()));
            for nonce in sim_order.nonces() {
                self.main_queue_nonces
                    .entry(nonce.address)
                    .or_default()
                    .push(sim_order.id());
            }
        } else {
            for pending_nonce in pending_nonces {
                let pending = self.pending_orders.entry(pending_nonce).or_default();
                if !pending.contains(&sim_order.id()) {
                    pending.push(sim_order.id());
                }
            }
            if !slots_ready {
                let id = sim_order.id();
                sim_order
                    .order
                    .target_storage_slots()
                    .iter()
                    .for_each(|slot| {
                        let pending = self.pending_on_slot.entry(slot.clone()).or_default();
                        if !pending.contains(&id) {
                            pending.push(id);
                        }
                    });
            }
        }
        self.orders_statistics.add(&sim_order.order);
        // We don't check the result to update orders_statistics since we already checked !self.orders.contains_key
        self.orders.insert(sim_order.id(), sim_order);
    }

    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        // we don't remove from pending because pending will clean itself
        if self.main_queue.remove(&id).is_some() {
            self.remove_poped_order(&id);
        }
        self.remove_from_orders(&id)
    }
}
