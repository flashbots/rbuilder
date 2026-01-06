use crate::evm_inspector::{SlotKey, UsedStateTrace};
use alloy_primitives::{Address, FixedBytes, B256};
use serde::Deserialize;
use std::collections::HashSet;

/// 4-byte function selector
pub type Selector = FixedBytes<4>;

/// Configuration for an ACE (Application Controlled Execution) protocol
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AceConfig {
    /// The primary contract address for this ACE protocol (used as unique identifier)
    pub contract_address: Address,
    /// Addresses that send ACE orders (used to identify force unlocks)
    pub from_addresses: HashSet<Address>,
    /// Addresses that receive ACE orders (the ACE contract addresses)
    pub to_addresses: HashSet<Address>,
    /// Storage slots that must be read to detect ACE interaction (e.g., _lastBlockUpdated at slot 3)
    pub detection_slots: HashSet<B256>,
    /// Function selectors (4 bytes) that indicate an unlock operation
    pub unlock_signatures: HashSet<Selector>,
    /// Function selectors (4 bytes) that indicate a forced unlock operation
    pub force_signatures: HashSet<Selector>,
}

/// Classify an ACE order interaction type based on state trace, simulation success, and config.
/// Uses both state trace (address access) AND function signatures to determine interaction type.
pub fn classify_ace_interaction(
    state_trace: &UsedStateTrace,
    sim_success: bool,
    config: &AceConfig,
    selector: Option<Selector>,
    tx_to: Option<Address>,
) -> Option<AceInteraction> {
    let any_ace_slots_accessed = config
        .to_addresses
        .iter()
        .flat_map(|address| {
            config.detection_slots.iter().map(|slot| SlotKey {
                address: *address,
                key: *slot,
            })
        })
        .flat_map(|key| {
            [
                state_trace.read_slot_values.contains_key(&key),
                state_trace.written_slot_values.contains_key(&key),
            ]
        })
        .any(|read_slot_of_interest| read_slot_of_interest);

    if !any_ace_slots_accessed {
        return None;
    }

    // Check if this is a direct call to the protocol
    let is_direct_protocol_call = tx_to.is_some_and(|to| config.to_addresses.contains(&to));

    // Check function selectors with direct HashSet lookup
    let is_force_sig = selector.is_some_and(|sel| config.force_signatures.contains(&sel));
    let is_unlock_sig = selector.is_some_and(|sel| config.unlock_signatures.contains(&sel));

    let contract_address = config.contract_address;

    if sim_success && (is_force_sig || is_unlock_sig) {
        let source = if is_direct_protocol_call && is_force_sig {
            AceUnlockSource::ProtocolForce
        } else if is_direct_protocol_call && is_unlock_sig {
            AceUnlockSource::ProtocolOptional
        } else {
            AceUnlockSource::User
        };
        Some(AceInteraction::Unlocking {
            contract_address,
            source,
        })
    } else {
        Some(AceInteraction::NonUnlocking { contract_address })
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
        contract_address: Address,
        source: AceUnlockSource,
    },
    /// Requires an unlocking ACE order, will revert otherwise
    NonUnlocking { contract_address: Address },
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

    pub fn get_contract_address(&self) -> Address {
        match self {
            AceInteraction::Unlocking {
                contract_address, ..
            }
            | AceInteraction::NonUnlocking { contract_address } => *contract_address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm_inspector::{SlotKey, UsedStateTrace};
    use alloy_primitives::hex;
    use alloy_primitives::{address, b256};

    /// Create the real ACE config from the provided TOML configuration
    fn real_ace_config() -> AceConfig {
        AceConfig {
            contract_address: address!("0000000aa232009084Bd71A5797d089AA4Edfad4"),
            from_addresses: HashSet::from([
                address!("c41ae140ca9b281d8a1dc254c50e446019517d04"),
                address!("d437f3372f3add2c2bc3245e6bd6f9c202e61bb3"),
                address!("693ca5c6852a7d212dabc98b28e15257465c11f3"),
            ]),
            to_addresses: HashSet::from([address!("0000000aa232009084Bd71A5797d089AA4Edfad4")]),
            // _lastBlockUpdated storage slot (slot 3)
            detection_slots: HashSet::from([b256!(
                "0000000000000000000000000000000000000000000000000000000000000003"
            )]),
            // unlockWithEmptyAttestation(address,bytes) nonpayable - 0x1828e0e7
            unlock_signatures: HashSet::from([Selector::from_slice(&[0x18, 0x28, 0xe0, 0xe7])]),
            // execute(bytes) nonpayable - 0x09c5eabe
            force_signatures: HashSet::from([Selector::from_slice(&[0x09, 0xc5, 0xea, 0xbe])]),
        }
    }

    /// Create a mock state trace with the detection slot accessed
    fn mock_state_trace_with_slot(addr: Address, slot: B256) -> UsedStateTrace {
        let mut trace = UsedStateTrace::default();
        trace.read_slot_values.insert(
            SlotKey {
                address: addr,
                key: slot,
            },
            Default::default(),
        );
        trace
    }

    #[test]
    fn test_real_ace_force_order_classification() {
        // Test with real force order calldata
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();
        let force_selector = *config.force_signatures.iter().next().unwrap();

        // Mock state trace with detection slot accessed
        let trace = mock_state_trace_with_slot(contract, detection_slot);

        // Direct call to ACE contract with force signature should be ProtocolForce
        let result =
            classify_ace_interaction(&trace, true, &config, Some(force_selector), Some(contract));

        assert_eq!(
            result,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::ProtocolForce
            })
        );

        // Verify it's detected as force
        assert!(result.unwrap().is_force());
        assert!(result.unwrap().is_protocol_tx());
    }

    #[test]
    fn test_real_ace_unlock_order_classification() {
        // Test with real unlock signature from config
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();
        let unlock_selector = *config.unlock_signatures.iter().next().unwrap();

        // Mock state trace with detection slot accessed
        let trace = mock_state_trace_with_slot(contract, detection_slot);

        // Direct call to ACE contract with unlock signature should be ProtocolOptional
        let result =
            classify_ace_interaction(&trace, true, &config, Some(unlock_selector), Some(contract));

        assert_eq!(
            result,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::ProtocolOptional
            })
        );

        // Verify it's protocol tx but not force
        assert!(result.unwrap().is_protocol_tx());
        assert!(!result.unwrap().is_force());
    }

    #[test]
    fn test_ace_user_unlock_indirect_call() {
        // User transaction that calls ACE contract indirectly (not tx.to = contract)
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();
        let unlock_selector = *config.unlock_signatures.iter().next().unwrap();

        let trace = mock_state_trace_with_slot(contract, detection_slot);

        // tx.to is NOT the ACE contract (indirect call via user tx) = User unlock
        let result = classify_ace_interaction(&trace, true, &config, Some(unlock_selector), None);

        assert_eq!(
            result,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::User
            })
        );

        // Verify it's unlocking but not a protocol tx
        assert!(result.unwrap().is_unlocking());
        assert!(!result.unwrap().is_protocol_tx());
    }

    #[test]
    fn test_ace_non_unlocking_interaction() {
        // Transaction that accesses ACE slot but doesn't have unlock signature
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();

        let trace = mock_state_trace_with_slot(contract, detection_slot);

        // No unlock/force signature = NonUnlocking
        let result = classify_ace_interaction(&trace, true, &config, None, Some(contract));

        assert_eq!(
            result,
            Some(AceInteraction::NonUnlocking {
                contract_address: contract
            })
        );

        // Verify it's not an unlocking interaction
        assert!(!result.unwrap().is_unlocking());
    }

    #[test]
    fn test_ace_failed_sim_becomes_non_unlocking() {
        // Even with unlock signature, failed simulation = NonUnlocking
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();
        let unlock_selector = *config.unlock_signatures.iter().next().unwrap();

        let trace = mock_state_trace_with_slot(contract, detection_slot);

        // sim_success = false turns unlock into NonUnlocking
        let result = classify_ace_interaction(
            &trace,
            false,
            &config,
            Some(unlock_selector),
            Some(contract),
        );

        assert_eq!(
            result,
            Some(AceInteraction::NonUnlocking {
                contract_address: contract
            })
        );

        // Failed sim should not be considered unlocking
        assert!(!result.unwrap().is_unlocking());
    }

    #[test]
    fn test_ace_no_slot_access_returns_none() {
        // If detection slot is not accessed, no ACE interaction detected
        let config = real_ace_config();
        let empty_trace = UsedStateTrace::default();
        let force_selector = *config.force_signatures.iter().next().unwrap();

        // Even with valid force signature, no slot access = None
        let result = classify_ace_interaction(
            &empty_trace,
            true,
            &config,
            Some(force_selector),
            Some(config.contract_address),
        );

        assert_eq!(
            result, None,
            "Should return None when detection slot is not accessed"
        );
    }

    #[test]
    fn test_ace_wrong_slot_returns_none() {
        // Accessing wrong slot should return None
        let config = real_ace_config();
        let wrong_slot = b256!("0000000000000000000000000000000000000000000000000000000000000099");
        let force_selector = *config.force_signatures.iter().next().unwrap();

        let trace = mock_state_trace_with_slot(config.contract_address, wrong_slot);

        // Wrong slot accessed = None (even with valid signature)
        let result = classify_ace_interaction(
            &trace,
            true,
            &config,
            Some(force_selector),
            Some(config.contract_address),
        );

        assert_eq!(
            result, None,
            "Should return None when wrong slot is accessed"
        );
    }

    #[test]
    fn test_ace_interaction_is_unlocking() {
        let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

        let force = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolForce,
        };
        assert!(force.is_unlocking());

        let optional = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolOptional,
        };
        assert!(optional.is_unlocking());

        let user = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::User,
        };
        assert!(user.is_unlocking());

        let non_unlocking = AceInteraction::NonUnlocking {
            contract_address: contract,
        };
        assert!(!non_unlocking.is_unlocking());
    }

    #[test]
    fn test_ace_interaction_is_protocol_tx() {
        let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

        let force = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolForce,
        };
        assert!(force.is_protocol_tx());

        let optional = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolOptional,
        };
        assert!(optional.is_protocol_tx());

        let user = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::User,
        };
        assert!(!user.is_protocol_tx());
    }

    #[test]
    fn test_ace_interaction_is_force() {
        let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

        let force = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolForce,
        };
        assert!(force.is_force());

        let optional = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolOptional,
        };
        assert!(!optional.is_force());

        let user = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::User,
        };
        assert!(!user.is_force());
    }

    #[test]
    fn test_ace_interaction_get_contract_address() {
        let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

        let unlocking = AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolForce,
        };
        assert_eq!(unlocking.get_contract_address(), contract);

        let non_unlocking = AceInteraction::NonUnlocking {
            contract_address: contract,
        };
        assert_eq!(non_unlocking.get_contract_address(), contract);
    }

    #[test]
    fn test_force_signature_from_real_calldata() {
        // The provided calldata starts with 0x09c5eabe (execute function)
        let calldata = hex::decode("09c5eabe000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000002950000cca0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000003e6d1e500000000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000003675af200000000000000000000008cff47e70a0000000000000000006f8f0c22bbdf00dac17f958d2ee523a2206206994597c13d831ec70000000000000000000000000000000000000000000000000000000001f548eb0000000000000000000000000000000000004c00000001000000000000000000000000000000000000003d82768a1dd582a9887587911fe9180001000200010000000000000000000000000000000000000000000000002b708452feb67dac0000660200000000000000000000004a458c968ab800000000000000000000000000000003ba0000000000000000010e8724bdb79c980300010000000000000000002548f28ce93ff600000000000000000000008cff47e70a000000000000000000225d82a810177e000108090000000000000000004a458c968ab80000000000000000000000000003e6ce2b000000000000000000000000000000010000000000000000000000000000000000001c2514149461c050689a85a1a293766501a00feab18c79d5b3cacb8c4052c9c0ea432416a6b9b672896d6596a3fa25fb765ab8a0245e2ebacfde1ed5a42786a6d60b00000000000000000025497f8c31270000000000000000000000000001f548eb00000000000000000000000003675af200000000000000000000000003675af200011c833917577a24b35aca558dcee9b4ab547c419f53a6b8b4e353e23ba811b956c35ef19655e4695da96e6e85f36c84db41d860eb7c267466cd1c0ebe581196086a0000000000000000000000000000").unwrap();

        // Extract first 4 bytes
        let selector = Selector::from_slice(&calldata[..4]);

        // Should match the force signature
        let expected_force_sig = Selector::from_slice(&[0x09, 0xc5, 0xea, 0xbe]);
        assert_eq!(selector, expected_force_sig);

        // Verify it's in the config
        let config = real_ace_config();
        assert!(config.force_signatures.contains(&selector));
    }

    #[test]
    fn test_optional_unlock_signature_from_real_transaction() {
        let calldata = hex::decode("1828e0e7000000000000000000000000c41ae140ca9b281d8a1dc254c50e446019517d0400000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000041c28cfd9fd7ffdce92022dcd0116088a1a0b1a9fb2124f55dce50ec39a10b9ad819f4ca93c677b0952c90389a4e1af98f9770fe4f3cdfa7b2fa30ecbd2c01a9bf1c00000000000000000000000000000000000000000000000000000000000000").unwrap();

        // Extract first 4 bytes (function selector)
        let selector = Selector::from_slice(&calldata[..4]);

        // Should match the optional unlock signature: unlockWithEmptyAttestation(address,bytes)
        let expected_unlock_sig = Selector::from_slice(&[0x18, 0x28, 0xe0, 0xe7]);
        assert_eq!(selector, expected_unlock_sig);

        // Verify it's in the config as an unlock signature
        let config = real_ace_config();
        assert!(config.unlock_signatures.contains(&selector));
        assert!(!config.force_signatures.contains(&selector)); // Should NOT be in force
    }

    #[test]
    fn test_optional_unlock_with_real_config() {
        // Test complete optional unlock classification with real config
        let config = real_ace_config();
        let contract = config.contract_address;
        let slot = b256!("0000000000000000000000000000000000000000000000000000000000000003");

        // Optional unlock signature from real transaction
        let unlock_selector = Selector::from_slice(&[0x18, 0x28, 0xe0, 0xe7]);

        // Mock state trace showing slot 3 was accessed
        let trace = mock_state_trace_with_slot(contract, slot);

        // Test 1: Direct call to ACE contract = ProtocolOptional
        let result =
            classify_ace_interaction(&trace, true, &config, Some(unlock_selector), Some(contract));

        assert_eq!(
            result,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::ProtocolOptional
            })
        );

        // Test 2: Indirect call (user tx) = User unlock
        let result_indirect =
            classify_ace_interaction(&trace, true, &config, Some(unlock_selector), None);

        assert_eq!(
            result_indirect,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::User
            })
        );

        // Test 3: Failed simulation with unlock signature = NonUnlocking
        let result_failed = classify_ace_interaction(
            &trace,
            false,
            &config,
            Some(unlock_selector),
            Some(contract),
        );

        assert_eq!(
            result_failed,
            Some(AceInteraction::NonUnlocking {
                contract_address: contract
            })
        );
    }

    #[test]
    fn test_slot_written_also_detected() {
        // Test that writing to the detection slot is also detected (not just reading)
        let config = real_ace_config();
        let contract = config.contract_address;
        let detection_slot = *config.detection_slots.iter().next().unwrap();
        let force_selector = *config.force_signatures.iter().next().unwrap();

        let mut trace = UsedStateTrace::default();
        // Write to slot instead of reading
        trace.written_slot_values.insert(
            SlotKey {
                address: contract,
                key: detection_slot,
            },
            Default::default(),
        );

        // Writing to detection slot should still trigger classification
        let result =
            classify_ace_interaction(&trace, true, &config, Some(force_selector), Some(contract));

        assert_eq!(
            result,
            Some(AceInteraction::Unlocking {
                contract_address: contract,
                source: AceUnlockSource::ProtocolForce
            })
        );
    }
}
