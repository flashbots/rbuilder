use crate::evm_inspector::UsedStateTrace;
use alloy_primitives::{Address, Bytes};
use derive_more::FromStr;
use serde::Deserialize;
use std::collections::HashSet;
use strum::EnumIter;

/// Configuration for an ACE (Atomic Clearing Engine) protocol
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AceConfig {
    /// Whether this ACE config is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Which ACE protocol this config is for
    pub protocol: AceExchange,
    /// Addresses that send ACE orders (used to identify force unlocks)
    pub from_addresses: HashSet<Address>,
    /// Addresses that receive ACE orders (the ACE contract addresses)
    pub to_addresses: HashSet<Address>,
    /// Function signatures that indicate an unlock operation
    pub unlock_signatures: HashSet<Bytes>,
    /// Function signatures that indicate a forced unlock operation
    pub force_signatures: HashSet<Bytes>,
}

fn default_enabled() -> bool {
    true
}

/// What ACE based exchanges that rbuilder supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter, Deserialize, FromStr)]
pub enum AceExchange {
    Angstrom,
}

impl AceExchange {
    /// Classify an ACE order interaction type based on state trace, simulation success, and config.
    /// Uses both state trace (address access) AND function signatures to determine interaction type.
    pub fn classify_ace_interaction(
        &self,
        state_trace: &UsedStateTrace,
        sim_success: bool,
        config: &AceConfig,
        selector: Option<&[u8]>,
    ) -> Option<AceInteraction> {
        match self {
            AceExchange::Angstrom => Self::angstrom_classify_interaction(
                state_trace,
                sim_success,
                *self,
                config,
                selector,
            ),
        }
    }

    /// Angstrom-specific classification logic using both state trace and signatures
    fn angstrom_classify_interaction(
        state_trace: &UsedStateTrace,
        sim_success: bool,
        exchange: AceExchange,
        config: &AceConfig,
        selector: Option<&[u8]>,
    ) -> Option<AceInteraction> {
        // Check state trace for ACE address access using config addresses
        let accessed_exchange = config.to_addresses.iter().any(|addr| {
            state_trace
                .read_slot_values
                .keys()
                .any(|k| &k.address == addr)
                || state_trace
                    .written_slot_values
                    .keys()
                    .any(|k| &k.address == addr)
        });

        if !accessed_exchange {
            return None;
        }

        // Check function signatures to determine if this is a force or regular unlock
        let is_force = selector.is_some_and(|sel| {
            config
                .force_signatures
                .iter()
                .any(|sig| sig.starts_with(sel))
        });

        let is_unlock = selector.is_some_and(|sel| {
            config
                .unlock_signatures
                .iter()
                .any(|sig| sig.starts_with(sel))
        });

        if sim_success && (is_force || is_unlock) {
            Some(AceInteraction::Unlocking { exchange, is_force })
        } else {
            Some(AceInteraction::NonUnlocking { exchange })
        }
    }
}

/// Type of ACE interaction for orders
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AceInteraction {
    /// Unlocking ACE order - doesn't revert without an ACE order, must be placed with ACE bundle.
    /// `is_force` indicates if this is a forced unlock (must always be included) vs optional.
    Unlocking {
        exchange: AceExchange,
        is_force: bool,
    },
    /// Requires an unlocking ACE order, will revert otherwise
    NonUnlocking { exchange: AceExchange },
}

impl AceInteraction {
    pub fn is_unlocking(&self) -> bool {
        matches!(self, Self::Unlocking { .. })
    }

    pub fn is_force(&self) -> bool {
        matches!(self, Self::Unlocking { is_force: true, .. })
    }

    pub fn get_exchange(&self) -> AceExchange {
        match self {
            AceInteraction::Unlocking { exchange, .. }
            | AceInteraction::NonUnlocking { exchange } => *exchange,
        }
    }
}

/// Type of unlock for ACE protocol transactions (Order::Ace)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AceUnlockType {
    /// Must unlock, transaction will fail if unlock conditions aren't met
    Force,
    /// Optional unlock, transaction can proceed with or without unlock
    Optional,
}
