use crate::evm_inspector::UsedStateTrace;
use alloy_primitives::{address, Address};
use strum::EnumIter;

/// What ace based exchanges that rbuilder supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum AceExchange {
    Angstrom,
}

impl AceExchange {
    /// Get the Angstrom variant
    pub const fn angstrom() -> Self {
        Self::Angstrom
    }

    /// Get the address for this exchange
    pub fn address(&self) -> Address {
        match self {
            AceExchange::Angstrom => address!("0000000aa232009084Bd71A5797d089AA4Edfad4"),
        }
    }

    /// Get the number of blocks this ACE exchange's transactions should be valid for
    pub fn blocks_to_live(&self) -> u64 {
        match self {
            AceExchange::Angstrom => 1,
        }
    }

    /// Classify an ACE transaction interaction type based on state trace and simulation success
    pub fn classify_ace_interaction(
        &self,
        state_trace: &UsedStateTrace,
        sim_success: bool,
    ) -> Option<AceInteraction> {
        match self {
            AceExchange::Angstrom => {
                Self::angstrom_classify_interaction(state_trace, sim_success, *self)
            }
        }
    }

    /// Angstrom-specific classification logic
    fn angstrom_classify_interaction(
        state_trace: &UsedStateTrace,
        sim_success: bool,
        exchange: AceExchange,
    ) -> Option<AceInteraction> {
        let angstrom_address = exchange.address();

        // We need to include read here as if it tries to reads the lastBlockUpdated on the pre swap
        // hook. it will revert and not make any changes if the pools not unlocked. We want to capture
        // this.
        let accessed_exchange = state_trace
            .read_slot_values
            .keys()
            .any(|k| k.address == angstrom_address)
            || state_trace
                .written_slot_values
                .keys()
                .any(|k| k.address == angstrom_address);

        accessed_exchange.then(|| {
            if sim_success {
                AceInteraction::Unlocking { exchange }
            } else {
                AceInteraction::NonUnlocking { exchange }
            }
        })
    }
}

/// Type of ACE interaction for mempool transactions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AceInteraction {
    /// Unlocking ACE tx,  doesn't revert without an ACE tx, must be placed with ACE bundle
    Unlocking { exchange: AceExchange },
    /// Requires an unlocking ACE tx, will revert otherwise
    NonUnlocking { exchange: AceExchange },
}

impl AceInteraction {
    pub fn is_unlocking(&self) -> bool {
        matches!(self, Self::Unlocking { .. })
    }

    pub fn get_exchange(&self) -> AceExchange {
        match self {
            AceInteraction::Unlocking { exchange } | AceInteraction::NonUnlocking { exchange } => {
                *exchange
            }
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
