use alloy_primitives::U256;
use rbuilder_primitives::{
    ace::{AceExchange, AceInteraction, AceUnlockType},
    Order, OrderId, SimulatedOrder,
};
use std::sync::Arc;
use tracing::{debug, trace};

/// The ACE bundler sits between the sim-tree and the builder itself. We put the bundler here as it
/// gives maximum flexibility for ACE protocols for defining ordering and handling cases were
/// certain tx's depend on other tx's. With this, a simple ace detection can be ran on incoming
/// orders. Before the orders get sent to the builders, Ace orders get intercepted here and then can
/// follow protocol specific ordering by leveraging the current bundling design. For example, if a
/// ace protocol wants to have a protocol transaction first and then sort everything greedly for there
/// protocol, there bundler can collect all the orders that interact with the protocol and then
/// generate a bundle with the protocol tx first with all other orders following and set to
/// droppable with a order that they want.
#[derive(Debug)]
pub struct AceBundler {
    /// ACE bundles organized by exchange
    exchanges: std::collections::HashMap<AceExchange, AceExchangeData>,
}

/// Data for a specific ACE exchange including all transaction types and logic
#[derive(Debug, Clone)]
pub struct AceExchangeData {
    /// Force ACE protocol tx - always included
    pub force_ace_tx: Option<AceOrderEntry>,
    /// Optional ACE protocol tx - conditionally included
    pub optional_ace_tx: Option<AceOrderEntry>,
    /// Mempool txs that unlock ACE state
    pub unlocking_mempool_txs: Vec<AceOrderEntry>,
    /// Mempool txs that require ACE unlock
    pub non_unlocking_mempool_txs: Vec<AceOrderEntry>,
}

#[derive(Debug, Clone)]
pub struct AceOrderEntry {
    pub order: Order,
    pub simulated: Arc<SimulatedOrder>,
    /// Profit after bundle simulation
    pub bundle_profit: U256,
}

impl AceExchangeData {
    /// Add an ACE protocol transaction
    pub fn add_ace_protocol_tx(
        &mut self,
        order: Order,
        simulated: Arc<SimulatedOrder>,
        unlock_type: AceUnlockType,
    ) {
        let entry = AceOrderEntry {
            order,
            bundle_profit: simulated.sim_value.full_profit_info().coinbase_profit(),
            simulated,
        };

        match unlock_type {
            AceUnlockType::Force => {
                self.force_ace_tx = Some(entry);
                trace!("Added forced ACE protocol unlock tx");
            }
            AceUnlockType::Optional => {
                self.optional_ace_tx = Some(entry);
                trace!("Added optional ACE protocol unlock tx");
            }
        }
    }

    /// Add a mempool ACE transaction
    pub fn add_mempool_tx(
        &mut self,
        order: Order,
        simulated: Arc<SimulatedOrder>,
        is_unlocking: bool,
    ) {
        let entry = AceOrderEntry {
            order,
            bundle_profit: simulated.sim_value.full_profit_info().coinbase_profit(),
            simulated,
        };

        if is_unlocking {
            self.unlocking_mempool_txs.push(entry);
            trace!("Added unlocking mempool ACE tx");
        } else {
            self.non_unlocking_mempool_txs.push(entry);
            trace!("Added non-unlocking mempool ACE tx");
        }
    }

    /// Check if we should include optional ACE protocol tx
    /// Optional is included if we have non-unlocking txs and no other unlock source
    fn should_include_optional(&self) -> bool {
        !self.non_unlocking_mempool_txs.is_empty()
            && self.force_ace_tx.is_none()
            && self.unlocking_mempool_txs.is_empty()
    }

    /// Check if we have an available unlock (either force ACE or mempool unlocking)
    fn has_unlock(&self) -> bool {
        self.force_ace_tx.is_some() || !self.unlocking_mempool_txs.is_empty()
    }

    /// Get the ACE bundle to place at top of block
    /// Returns all unlock txs (force ACE, optional ACE, mempool unlocks) followed by non-unlocking txs
    pub fn get_ace_bundle(&self) -> Vec<Order> {
        let mut orders = Vec::new();

        // Priority 1: Force ACE unlock (always included)
        if let Some(ref force_tx) = self.force_ace_tx {
            orders.push(force_tx.order.clone());
        }

        // Priority 2: Optional ACE unlock (if needed and no force ACE)
        if let Some(ref optional_tx) = self.optional_ace_tx {
            if self.should_include_optional() {
                orders.push(optional_tx.order.clone());
            }
        }

        // Priority 3: Mempool unlocking txs
        for entry in &self.unlocking_mempool_txs {
            orders.push(entry.order.clone());
        }

        // Priority 4: Non-unlocking mempool txs (only if we have an unlock)
        if self.has_unlock() || self.should_include_optional() {
            for entry in &self.non_unlocking_mempool_txs {
                orders.push(entry.order.clone());
            }
        }

        orders
    }

    /// Update profits and sort by profitability
    pub fn update_profits(&mut self, order_id: &OrderId, profit: U256) -> bool {
        if let Some(ref mut entry) = self.force_ace_tx {
            if entry.order.id() == *order_id {
                entry.bundle_profit = profit;
                return true;
            }
        }

        if let Some(ref mut entry) = self.optional_ace_tx {
            if entry.order.id() == *order_id {
                entry.bundle_profit = profit;
                return true;
            }
        }

        for entry in &mut self.unlocking_mempool_txs {
            if entry.order.id() == *order_id {
                entry.bundle_profit = profit;
                return true;
            }
        }

        for entry in &mut self.non_unlocking_mempool_txs {
            if entry.order.id() == *order_id {
                entry.bundle_profit = profit;
                return true;
            }
        }

        false
    }

    /// Sort mempool transactions by profitability
    pub fn sort_by_profit(&mut self) {
        self.unlocking_mempool_txs
            .sort_by(|a, b| b.bundle_profit.cmp(&a.bundle_profit));
        self.non_unlocking_mempool_txs
            .sort_by(|a, b| b.bundle_profit.cmp(&a.bundle_profit));
    }

    /// Remove orders that builder wants to kick out
    pub fn kick_out_orders(&mut self, order_ids: &[OrderId]) {
        if let Some(ref force_tx) = self.force_ace_tx {
            if order_ids.contains(&force_tx.order.id()) {
                debug!("Attempted to kick out force ACE tx - ignoring");
            }
        }

        self.unlocking_mempool_txs
            .retain(|entry| !order_ids.contains(&entry.order.id()));
        self.non_unlocking_mempool_txs
            .retain(|entry| !order_ids.contains(&entry.order.id()));
    }

    /// Get total profit
    pub fn total_profit(&self) -> U256 {
        let mut total = U256::ZERO;

        if let Some(ref entry) = self.force_ace_tx {
            total = total.saturating_add(entry.bundle_profit);
        }
        if let Some(ref entry) = self.optional_ace_tx {
            total = total.saturating_add(entry.bundle_profit);
        }

        for entry in &self.unlocking_mempool_txs {
            total = total.saturating_add(entry.bundle_profit);
        }
        for entry in &self.non_unlocking_mempool_txs {
            total = total.saturating_add(entry.bundle_profit);
        }

        total
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.force_ace_tx.is_none()
            && self.optional_ace_tx.is_none()
            && self.unlocking_mempool_txs.is_empty()
            && self.non_unlocking_mempool_txs.is_empty()
    }

    /// Get count of orders
    pub fn len(&self) -> usize {
        let mut count = 0;
        if self.force_ace_tx.is_some() {
            count += 1;
        }
        if self.optional_ace_tx.is_some() {
            count += 1;
        }
        count + self.unlocking_mempool_txs.len() + self.non_unlocking_mempool_txs.len()
    }
}

impl AceBundler {
    pub fn new() -> Self {
        Self {
            exchanges: std::collections::HashMap::new(),
        }
    }

    /// Add an ACE protocol transaction (Order::Ace)
    pub fn add_ace_protocol_tx(
        &mut self,
        order: Order,
        simulated: Arc<SimulatedOrder>,
        unlock_type: AceUnlockType,
        exchange: AceExchange,
    ) {
        let data = self.exchanges.entry(exchange).or_default();
        data.add_ace_protocol_tx(order, simulated, unlock_type);
    }

    /// Add a mempool ACE transaction or bundle containing ACE interactions
    pub fn add_mempool_ace_tx(
        &mut self,
        order: Order,
        simulated: Arc<SimulatedOrder>,
        interaction: AceInteraction,
    ) {
        if matches!(order, Order::Bundle(_) | Order::ShareBundle(_)) {
            trace!(
                order_id = ?order.id(),
                "Adding ACE bundle/share bundle - will be treated as atomic unit"
            );
        }

        match interaction {
            AceInteraction::Unlocking { exchange } => {
                let data = self.exchanges.entry(exchange).or_default();
                data.add_mempool_tx(order, simulated, true);
            }
            AceInteraction::NonUnlocking { exchange } => {
                let data = self.exchanges.entry(exchange).or_default();
                data.add_mempool_tx(order, simulated, false);
            }
        }
    }

    /// Handle replacement of a mempool transaction
    pub fn replace_mempool_tx(
        &mut self,
        old_order_id: &OrderId,
        new_order: Order,
        new_simulated: Arc<SimulatedOrder>,
        interaction: AceInteraction,
    ) -> bool {
        let mut found = false;
        for data in self.exchanges.values_mut() {
            if let Some(pos) = data
                .unlocking_mempool_txs
                .iter()
                .position(|e| e.order.id() == *old_order_id)
            {
                data.unlocking_mempool_txs.remove(pos);
                found = true;
                break;
            }
            if let Some(pos) = data
                .non_unlocking_mempool_txs
                .iter()
                .position(|e| e.order.id() == *old_order_id)
            {
                data.non_unlocking_mempool_txs.remove(pos);
                found = true;
                break;
            }
        }

        if found {
            self.add_mempool_ace_tx(new_order, new_simulated, interaction);
            trace!(
                "Replaced ACE mempool tx {:?} with new version",
                old_order_id
            );
        }

        found
    }

    /// Get the ACE bundle for a specific exchange to place at top of block
    pub fn get_ace_bundle(&self, exchange: &AceExchange) -> Vec<Order> {
        self.exchanges
            .get(exchange)
            .map(|data| data.get_ace_bundle())
            .unwrap_or_default()
    }

    /// Update profits after bundle simulation
    pub fn update_after_simulation(&mut self, simulation_results: Vec<(OrderId, U256)>) {
        for (order_id, profit) in simulation_results {
            for data in self.exchanges.values_mut() {
                if data.update_profits(&order_id, profit) {
                    break;
                }
            }
        }

        // Sort all exchanges by profit
        for data in self.exchanges.values_mut() {
            data.sort_by_profit();
        }
    }

    /// Remove specific ACE orders if builder has better alternatives
    pub fn kick_out_orders(&mut self, exchange: &AceExchange, order_ids: &[OrderId]) {
        if let Some(data) = self.exchanges.get_mut(exchange) {
            data.kick_out_orders(order_ids);
        }
    }

    /// Get all configured exchanges
    pub fn get_exchanges(&self) -> Vec<AceExchange> {
        self.exchanges.keys().cloned().collect()
    }

    /// Clear all orders
    pub fn clear(&mut self) {
        self.exchanges.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty() || self.exchanges.values().all(|d| d.is_empty())
    }

    pub fn len(&self) -> usize {
        self.exchanges.values().map(|d| d.len()).sum()
    }

    /// Get total profit for a specific exchange
    pub fn total_profit(&self, exchange: &AceExchange) -> U256 {
        self.exchanges
            .get(exchange)
            .map(|d| d.total_profit())
            .unwrap_or(U256::ZERO)
    }
}

impl Default for AceExchangeData {
    fn default() -> Self {
        Self {
            force_ace_tx: None,
            optional_ace_tx: None,
            unlocking_mempool_txs: Vec::new(),
            non_unlocking_mempool_txs: Vec::new(),
        }
    }
}
