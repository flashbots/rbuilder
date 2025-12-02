use crate::evm_inspector::UsedStateTrace;
use alloy_primitives::{Address, Bytes, B256};
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
    /// Storage slots that must be read to detect ACE interaction (e.g., _lastBlockUpdated at slot 3)
    pub detection_slots: HashSet<B256>,
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
        tx_to: Option<Address>,
    ) -> Option<AceInteraction> {
        match self {
            AceExchange::Angstrom => Self::angstrom_classify_interaction(
                state_trace,
                sim_success,
                *self,
                config,
                selector,
                tx_to,
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
        tx_to: Option<Address>,
    ) -> Option<AceInteraction> {
        // Check that ALL detection slots are read or written from any of the ACE contract addresses
        let all_slots_accessed = config.detection_slots.iter().all(|slot| {
            config.to_addresses.iter().any(|addr| {
                state_trace
                    .read_slot_values
                    .keys()
                    .any(|k| &k.address == addr && &k.key == slot)
                    || state_trace
                        .written_slot_values
                        .keys()
                        .any(|k| &k.address == addr && &k.key == slot)
            })
        });

        if !all_slots_accessed {
            return None;
        }

        // Check if this is a direct call to the protocol
        let is_direct_protocol_call = tx_to.is_some_and(|to| config.to_addresses.contains(&to));

        // Check function signatures to determine if this is a force or regular unlock
        let is_force_sig = selector.is_some_and(|sel| {
            config
                .force_signatures
                .iter()
                .any(|sig| sig.starts_with(sel))
        });

        let is_unlock_sig = selector.is_some_and(|sel| {
            config
                .unlock_signatures
                .iter()
                .any(|sig| sig.starts_with(sel))
        });

        if sim_success && (is_force_sig || is_unlock_sig) {
            let source = if is_direct_protocol_call && is_force_sig {
                AceUnlockSource::ProtocolForce
            } else if is_direct_protocol_call && is_unlock_sig {
                AceUnlockSource::ProtocolOptional
            } else {
                AceUnlockSource::User
            };
            Some(AceInteraction::Unlocking { exchange, source })
        } else {
            Some(AceInteraction::NonUnlocking { exchange })
        }
    }
}

/// Source of an ACE unlock order
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AceUnlockSource {
    /// Direct call to protocol with force signature - must always be included
    ProtocolForce,
    /// Direct call to protocol with optional unlock signature
    ProtocolOptional,
    /// Indirect interaction (user tx that interacts with ACE contract)
    User,
}

/// Type of ACE interaction for orders
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AceInteraction {
    /// Unlocking ACE order - doesn't revert without an ACE order, must be placed with ACE bundle.
    Unlocking {
        exchange: AceExchange,
        source: AceUnlockSource,
    },
    /// Requires an unlocking ACE order, will revert otherwise
    NonUnlocking { exchange: AceExchange },
}

impl AceInteraction {
    pub fn is_unlocking(&self) -> bool {
        matches!(self, Self::Unlocking { .. })
    }

    pub fn is_protocol_tx(&self) -> bool {
        matches!(
            self,
            Self::Unlocking {
                source: AceUnlockSource::ProtocolForce | AceUnlockSource::ProtocolOptional,
                ..
            }
        )
    }

    pub fn is_force(&self) -> bool {
        matches!(
            self,
            Self::Unlocking {
                source: AceUnlockSource::ProtocolForce,
                ..
            }
        )
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
