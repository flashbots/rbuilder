use alloy_primitives::U256;
use itertools::Itertools;
use rbuilder_primitives::{
    ace::{AceExchange, AceInteraction, AceUnlockType},
    Order, SimulatedOrder,
};
use std::sync::Arc;
use tracing::trace;

use crate::{building::sim::SimulationRequest, live_builder::simulation::SimulatedOrderCommand};

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
    exchanges: ahash::HashMap<AceExchange, AceExchangeData>,
}

/// Data for a specific ACE exchange including all transaction types and logic
#[derive(Debug, Clone)]
pub struct AceExchangeData {
    /// Force ACE protocol tx - always included
    pub force_ace_tx: Option<AceOrderEntry>,
    /// Optional ACE protocol tx - conditionally included
    pub optional_ace_tx: Option<AceOrderEntry>,
    /// weather or not we have pushed through an unlocking mempool tx.
    pub has_unlocking: bool,
    /// Mempool txs that require ACE unlock
    pub non_unlocking_mempool_txs: Vec<AceOrderEntry>,
}

#[derive(Debug, Clone)]
pub struct AceOrderEntry {
    pub simulated: Arc<SimulatedOrder>,
    /// Profit after bundle simulation
    pub bundle_profit: U256,
}

impl AceExchangeData {
    /// Add an ACE protocol transaction
    pub fn add_ace_protocol_tx(
        &mut self,
        simulated: Arc<SimulatedOrder>,
        unlock_type: AceUnlockType,
    ) -> Vec<SimulationRequest> {
        let sim_cpy = simulated.order.clone();

        let entry = AceOrderEntry {
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

        // Take all non-unlocking orders and simulate them with parents so they will pass and inject
        // them into the system.
        self.non_unlocking_mempool_txs
            .drain(..)
            .map(|entry| SimulationRequest {
                id: rand::random(),
                order: entry.simulated.order.clone(),
                parents: vec![sim_cpy.clone()],
            })
            .collect_vec()
    }

    pub fn try_generate_sim_request(&self, order: &Order) -> Option<SimulationRequest> {
        let Some(parent) = self
            .optional_ace_tx
            .as_ref()
            .or_else(|| self.force_ace_tx.as_ref())
        else {
            return None;
        };

        Some(SimulationRequest {
            id: rand::random(),
            order: order.clone(),
            parents: vec![parent.simulated.order.clone()],
        })
    }

    // If we have a regular mempool unlocking tx, we don't want to include the optional ace
    // transaction ad will cancel it.
    pub fn has_unlocking(&mut self) -> Option<SimulatedOrderCommand> {
        self.has_unlocking = true;

        self.optional_ace_tx
            .take()
            .map(|order| SimulatedOrderCommand::Cancellation(order.simulated.order.id()))
    }

    pub fn add_mempool_tx(&mut self, simulated: Arc<SimulatedOrder>) -> Option<SimulationRequest> {
        if let Some(req) = self.try_generate_sim_request(&simulated.order) {
            return Some(req);
        }
        // we don't have a way to sim this mempool tx yet, going to collect it instead.

        let entry = AceOrderEntry {
            bundle_profit: simulated.sim_value.full_profit_info().coinbase_profit(),
            simulated,
        };

        trace!("Added non-unlocking mempool ACE tx");
        self.non_unlocking_mempool_txs.push(entry);

        None
    }
}

impl AceBundler {
    pub fn new() -> Self {
        Self {
            exchanges: ahash::HashMap::default(),
        }
    }

    /// Add an ACE protocol transaction (Order::Ace)
    pub fn add_ace_protocol_tx(
        &mut self,
        simulated: Arc<SimulatedOrder>,
        unlock_type: AceUnlockType,
        exchange: AceExchange,
    ) {
        let data = self.exchanges.entry(exchange).or_default();
        data.add_ace_protocol_tx(simulated, unlock_type);
    }

    pub fn have_unlocking(&mut self, exchange: AceExchange) -> Option<SimulatedOrderCommand> {
        self.exchanges.entry(exchange).or_default().has_unlocking()
    }

    /// Add a mempool ACE transaction or bundle containing ACE interactions
    pub fn add_mempool_ace_tx(
        &mut self,
        simulated: Arc<SimulatedOrder>,
        interaction: AceInteraction,
    ) -> Option<SimulationRequest> {
        self.exchanges
            .entry(interaction.get_exchange())
            .or_default()
            .add_mempool_tx(simulated)
    }

    /// Get all configured exchanges
    pub fn get_exchanges(&self) -> Vec<AceExchange> {
        self.exchanges.keys().cloned().collect()
    }

    /// Clear all orders
    pub fn clear(&mut self) {
        self.exchanges.clear();
    }
}

impl Default for AceExchangeData {
    fn default() -> Self {
        Self {
            force_ace_tx: None,
            optional_ace_tx: None,
            has_unlocking: false,
            non_unlocking_mempool_txs: Vec::new(),
        }
    }
}
