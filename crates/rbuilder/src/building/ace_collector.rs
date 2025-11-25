use ahash::HashMap;
use alloy_primitives::U256;
use alloy_rpc_types::TransactionTrait;
use itertools::Itertools;
use rbuilder_primitives::{
    ace::{AceExchange, AceInteraction, AceUnlockType},
    Order, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs,
};
use std::sync::Arc;
use tracing::trace;

use crate::{
    building::sim::SimulationRequest,
    live_builder::{config::AceConfig, simulation::SimulatedOrderCommand},
};

/// Collects Ace Orders
#[derive(Debug, Default)]
pub struct AceCollector {
    /// ACE bundles organized by exchange
    exchanges: ahash::HashMap<AceExchange, AceExchangeData>,
    ace_tx_lookup: ahash::HashMap<AceExchange, AceConfig>,
}

impl AceConfig {
    pub fn is_ace_force(&self, tx: &TransactionSignedEcRecoveredWithBlobs) -> bool {
        let internal = tx.internal_tx_unsecure();
        self.from_addresses.contains(&internal.signer())
            && self
                .to_addresses
                .contains(&internal.inner().to().unwrap_or_default())
    }
}

/// Data for a specific ACE exchange including all transaction types and logic
#[derive(Debug, Clone, Default)]
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
    fn add_ace_protocol_tx(
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

    fn try_generate_sim_request(&self, order: &Order) -> Option<SimulationRequest> {
        let parent = self
            .optional_ace_tx
            .as_ref()
            .or(self.force_ace_tx.as_ref())?;

        Some(SimulationRequest {
            id: rand::random(),
            order: order.clone(),
            parents: vec![parent.simulated.order.clone()],
        })
    }

    // If we have a regular mempool unlocking tx, we don't want to include the optional ace
    // transaction ad will cancel it.
    fn has_unlocking(&mut self) -> Option<SimulatedOrderCommand> {
        // we only want to send this once.
        if self.has_unlocking {
            return None;
        }

        self.has_unlocking = true;

        self.optional_ace_tx
            .take()
            .map(|order| SimulatedOrderCommand::Cancellation(order.simulated.order.id()))
    }

    fn add_mempool_tx(&mut self, simulated: Arc<SimulatedOrder>) -> Option<SimulationRequest> {
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

impl AceCollector {
    pub fn new(config: Vec<AceConfig>) -> Self {
        let mut lookup = HashMap::default();
        let mut exchanges = HashMap::default();

        for ace in config {
            let protocol = ace.protocol;
            lookup.insert(protocol, ace);
            exchanges.insert(protocol, Default::default());
        }

        Self {
            exchanges,
            ace_tx_lookup: lookup,
        }
    }

    pub fn is_ace_force(&self, order: &Order) -> bool {
        match order {
            Order::Tx(tx) => self
                .ace_tx_lookup
                .values()
                .any(|config| config.is_ace_force(&tx.tx_with_blobs)),
            _ => false,
        }
    }

    pub fn add_ace_protocol_tx(
        &mut self,
        simulated: Arc<SimulatedOrder>,
        unlock_type: AceUnlockType,
        exchange: AceExchange,
    ) -> Vec<SimulationRequest> {
        let data = self.exchanges.entry(exchange).or_default();

        data.add_ace_protocol_tx(simulated, unlock_type)
    }

    pub fn has_unlocking(&self, exchange: &AceExchange) -> bool {
        self.exchanges
            .get(exchange)
            .map(|e| e.has_unlocking)
            .unwrap_or_default()
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
