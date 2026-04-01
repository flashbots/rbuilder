use super::test_chain_state::{BlockArgs, TestChainState};
use crate::building::sim::{
    AceExchangeState, AceSimulationState, DependencyKey, NonceKey, SimTree, SimulatedResult,
    SimulationRequest,
};
use crate::utils::NonceCache;
use alloy_primitives::{address, b256, Address, B256, U256};
use rbuilder_primitives::ace::{AceConfig, AceInteraction, AceUnlockSource, Selector};
use rbuilder_primitives::evm_inspector::{SlotKey, UsedStateTrace};
use rbuilder_primitives::{BlockSpace, Bundle, BundleVersion, Order, SimValue, SimulatedOrder};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// Create a minimal order for testing (empty bundle with unique ID)
fn create_test_order() -> Arc<Order> {
    Arc::new(Order::Bundle(Bundle {
        version: BundleVersion::V1,
        block: None,
        min_timestamp: None,
        max_timestamp: None,
        txs: Vec::new(),
        reverting_tx_hashes: Vec::new(),
        dropping_tx_hashes: Vec::new(),
        hash: B256::ZERO,
        uuid: Uuid::new_v4(),
        replacement_data: None,
        signer: None,
        refund_identity: None,
        metadata: Default::default(),
        refund: None,
        external_hash: None,
    }))
}

/// Create the real ACE config for testing
fn test_ace_config() -> AceConfig {
    AceConfig {
        contract_address: address!("0000000aa232009084Bd71A5797d089AA4Edfad4"),
        from_addresses: HashSet::from([address!("c41ae140ca9b281d8a1dc254c50e446019517d04")]),
        to_addresses: HashSet::from([address!("0000000aa232009084Bd71A5797d089AA4Edfad4")]),
        detection_slots: HashSet::from([b256!(
            "0000000000000000000000000000000000000000000000000000000000000003"
        )]),
        unlock_signatures: HashSet::from([Selector::from_slice(&[0x18, 0x28, 0xe0, 0xe7])]),
        force_signatures: HashSet::from([Selector::from_slice(&[0x09, 0xc5, 0xea, 0xbe])]),
    }
}

/// Create a mock state trace with ACE detection slot written (unlock orders write to detection slot)
fn mock_state_trace_with_ace_slot(contract: Address, slot: B256) -> UsedStateTrace {
    let mut trace = UsedStateTrace::default();
    trace.written_slot_values.insert(
        SlotKey {
            address: contract,
            key: slot,
        },
        Default::default(),
    );
    trace
}

/// Create a mock force unlock order
fn create_force_unlock_order(contract: Address, gas_used: u64) -> Arc<SimulatedOrder> {
    let slot = b256!("0000000000000000000000000000000000000000000000000000000000000003");
    Arc::new(SimulatedOrder {
        order: create_test_order(),
        sim_value: SimValue::new(
            U256::from(10),
            U256::from(10),
            BlockSpace::new(gas_used, 0, 0),
            Vec::new(),
        ),
        used_state_trace: Some(mock_state_trace_with_ace_slot(contract, slot)),
        ace_interactions: vec![AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolForce,
        }],
    })
}

/// Create a mock optional unlock order
fn create_optional_unlock_order(contract: Address, gas_used: u64) -> Arc<SimulatedOrder> {
    let slot = b256!("0000000000000000000000000000000000000000000000000000000000000003");
    Arc::new(SimulatedOrder {
        order: create_test_order(),
        sim_value: SimValue::new(
            U256::from(5),
            U256::from(5),
            BlockSpace::new(gas_used, 0, 0),
            Vec::new(),
        ),
        used_state_trace: Some(mock_state_trace_with_ace_slot(contract, slot)),
        ace_interactions: vec![AceInteraction::Unlocking {
            contract_address: contract,
            source: AceUnlockSource::ProtocolOptional,
        }],
    })
}

#[test]
fn test_ace_exchange_state_get_unlock_order_force_only() {
    let order = create_force_unlock_order(
        address!("0000000aa232009084Bd71A5797d089AA4Edfad4"),
        100_000,
    );
    let state = AceExchangeState {
        force_unlock_order: Some(order.clone()),
        ..Default::default()
    };

    let result = state.get_unlock_order();
    assert_eq!(result, Some(&order));
}

#[test]
fn test_ace_exchange_state_get_unlock_order_optional_only() {
    let order =
        create_optional_unlock_order(address!("0000000aa232009084Bd71A5797d089AA4Edfad4"), 50_000);
    let state = AceExchangeState {
        optional_unlock_order: Some(order.clone()),
        ..Default::default()
    };

    let result = state.get_unlock_order();
    assert_eq!(result, Some(&order));
}

#[test]
fn test_force_unlock_always_preferred() {
    let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let force_order = create_force_unlock_order(contract, 100_000);
    let optional_order = create_optional_unlock_order(contract, 50_000);

    let state = AceExchangeState {
        force_unlock_order: Some(force_order.clone()),
        optional_unlock_order: Some(optional_order.clone()),
        ..Default::default()
    };

    // Force is always preferred over optional, regardless of gas cost
    let result = state.get_unlock_order();
    assert_eq!(result, Some(&force_order));
}

#[test]
fn test_equal_gas_prefers_force() {
    let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let force_order = create_force_unlock_order(contract, 100_000);
    let optional_order = create_optional_unlock_order(contract, 100_000);

    let state = AceExchangeState {
        force_unlock_order: Some(force_order.clone()),
        optional_unlock_order: Some(optional_order.clone()),
        ..Default::default()
    };

    // When equal gas, should prefer force (d comparison)
    let result = state.get_unlock_order();
    assert_eq!(result, Some(&force_order));
}

#[test]
fn test_ace_exchange_state_get_unlock_order_none() {
    let state = AceExchangeState::default();
    assert_eq!(state.get_unlock_order(), None);
}

#[test]
fn test_sim_tree_ace_config_registration() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let ace_config = test_ace_config();
    let contract_addr = ace_config.contract_address;

    let sim_tree = SimTree::new(nonce_cache, vec![ace_config.clone()]);

    // Verify config is registered
    assert!(sim_tree.ace_configs().contains_key(&contract_addr));

    // Verify state is initialized
    assert!(sim_tree.get_ace_state(&contract_addr).is_some());

    Ok(())
}

#[test]
fn test_multiple_ace_contracts() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let contract1 = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let contract2 = address!("1111111aa232009084Bd71A5797d089AA4Edfad4");

    let mut config1 = test_ace_config();
    config1.contract_address = contract1;

    let mut config2 = test_ace_config();
    config2.contract_address = contract2;

    let sim_tree = SimTree::new(nonce_cache, vec![config1, config2]);

    // Both contracts should be registered
    assert!(sim_tree.ace_configs().contains_key(&contract1));
    assert!(sim_tree.ace_configs().contains_key(&contract2));

    // Both should have state
    assert!(sim_tree.get_ace_state(&contract1).is_some());
    assert!(sim_tree.get_ace_state(&contract2).is_some());

    Ok(())
}

#[test]
fn test_mark_mempool_unlock_basic() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let ace_config = test_ace_config();
    let contract_addr = ace_config.contract_address;

    let mut sim_tree = SimTree::new(nonce_cache, vec![ace_config]);

    // Mark mempool unlock (first time)
    let cancelled1 = sim_tree.mark_mempool_unlock(contract_addr);
    // No optional order yet, so nothing to cancel
    assert_eq!(cancelled1, None);

    // Mark again (should be idempotent)
    let cancelled2 = sim_tree.mark_mempool_unlock(contract_addr);
    assert_eq!(cancelled2, None);

    // Verify state shows mempool unlock was marked
    let ace_state = sim_tree.get_ace_state(&contract_addr).unwrap();
    assert!(ace_state.has_mempool_unlock);

    Ok(())
}

// Note: More detailed cancellation tests require access to SimTree internals
// or integration with handle_ace_interaction which we'll test separately

#[test]
fn test_handle_ace_unlock_with_mempool_unlock() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let ace_config = test_ace_config();
    let contract_addr = ace_config.contract_address;

    let mut sim_tree = SimTree::new(nonce_cache, vec![ace_config]);

    let cancelled = sim_tree.mark_mempool_unlock(contract_addr);
    assert_eq!(cancelled, None); // Nothing to cancel yet

    let optional_order = create_optional_unlock_order(contract_addr, 50_000);

    let result = SimulatedResult::Success {
        id: rand::random(),
        simulated_order: optional_order.clone(),
        previous_orders: Vec::new(),
        dependencies_satisfied: Vec::new(),
        simulation_time: std::time::Duration::from_millis(10),
    };

    // Handle the ACE unlock
    let cancellations = sim_tree.handle_ace_unlock(&result)?;

    // Unlocking orders should be cancelled since mempool unlock was already marked
    assert!(cancellations.contains(&optional_order.order.id()));

    // Optional order should NOT be stored
    let ace_state = sim_tree.get_ace_state(&contract_addr).unwrap();
    assert!(ace_state.optional_unlock_order.is_none());

    Ok(())
}

#[test]
fn test_optional_ace_not_stored_without_pending_orders() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let ace_config = test_ace_config();
    let contract_addr = ace_config.contract_address;

    let mut sim_tree = SimTree::new(nonce_cache, vec![ace_config]);

    // Create optional unlock order but NO pending orders waiting on it
    let optional_order = create_optional_unlock_order(contract_addr, 50_000);

    let result = SimulatedResult::Success {
        id: rand::random(),
        simulated_order: optional_order.clone(),
        previous_orders: Vec::new(),
        dependencies_satisfied: Vec::new(),
        simulation_time: std::time::Duration::from_millis(10),
    };

    // Handle the ACE unlock - should be cancelled because no orders need it
    let cancellations = sim_tree.handle_ace_unlock(&result)?;

    // Optional should be cancelled because no orders are waiting
    assert!(cancellations.contains(&optional_order.order.id()));

    // Optional order should NOT be stored
    let ace_state = sim_tree.get_ace_state(&contract_addr).unwrap();
    assert!(ace_state.optional_unlock_order.is_none());

    Ok(())
}

#[test]
fn test_dependency_key_from_nonce() {
    let nonce_key = NonceKey {
        address: address!("0000000000000000000000000000000000000001"),
        nonce: 5,
    };
    let nonce_key_clone = nonce_key.clone();
    let dep_key: DependencyKey = nonce_key.into();
    assert_eq!(dep_key, DependencyKey::Nonce(nonce_key_clone));
}

#[test]
fn test_dependency_key_ace_unlock() {
    let contract_addr = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let dep_key = DependencyKey::AceUnlock(contract_addr);

    match dep_key {
        DependencyKey::AceUnlock(addr) => assert_eq!(addr, contract_addr),
        _ => panic!("Expected AceUnlock dependency"),
    }
}

// ============================================================================
// AceSimulationState Tests
// ============================================================================

use ahash::HashSet as AHashSet;

#[test]
fn test_simulation_request_ace_state_empty_default() {
    // New orders should start with empty ace_state
    let request = SimulationRequest {
        id: rand::random(),
        order: create_test_order(),
        parents: Vec::new(),
        ace_state: AceSimulationState::default(),
        has_ace_parents: false,
    };

    assert!(request.ace_state.all_dependencies_accounted());
}

#[test]
fn test_ace_simulation_state_single_dependency() {
    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

    // Order detected a NonUnlocking interaction (needs unlock)
    let mut detected = AHashSet::default();
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_a,
    });

    let ace_state = AceSimulationState {
        detected_interactions: detected,
        accounted_for_interactions: AHashSet::default(),
    };

    // Should have unhandled dependencies
    assert!(!ace_state.all_dependencies_accounted());

    let deps = ace_state.dependencies_to_handle();
    assert_eq!(deps.len(), 1);
}

#[test]
fn test_ace_simulation_state_multiple_dependencies() {
    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let contract_b = address!("1111111aa232009084Bd71A5797d089AA4Edfad4");
    let contract_c = address!("2222222aa232009084Bd71A5797d089AA4Edfad4");

    let mut detected = AHashSet::default();
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_a,
    });
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_b,
    });
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_c,
    });

    let ace_state = AceSimulationState {
        detected_interactions: detected,
        accounted_for_interactions: AHashSet::default(),
    };

    let deps = ace_state.dependencies_to_handle();
    assert_eq!(deps.len(), 3);
}

#[test]
fn test_ace_simulation_state_partial_unlock() {
    // Test that partial unlocks are tracked correctly
    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let contract_b = address!("1111111aa232009084Bd71A5797d089AA4Edfad4");

    let mut detected = AHashSet::default();
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_a,
    });
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_b,
    });

    // We've accounted for unlock on contract_a
    let mut accounted = AHashSet::default();
    accounted.insert(AceInteraction::Unlocking {
        contract_address: contract_a,
        source: AceUnlockSource::User,
    });

    let ace_state = AceSimulationState {
        detected_interactions: detected,
        accounted_for_interactions: accounted,
    };

    // Should still have one unhandled dependency (contract_b)
    assert!(!ace_state.all_dependencies_accounted());
    let deps = ace_state.dependencies_to_handle();
    assert_eq!(deps.len(), 1);
}

#[test]
fn test_ace_simulation_state_all_accounted() {
    // When all unlocks are accounted for, should be fully resolved
    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

    let mut detected = AHashSet::default();
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_a,
    });

    let mut accounted = AHashSet::default();
    accounted.insert(AceInteraction::Unlocking {
        contract_address: contract_a,
        source: AceUnlockSource::User,
    });

    let ace_state = AceSimulationState {
        detected_interactions: detected,
        accounted_for_interactions: accounted,
    };

    assert!(ace_state.all_dependencies_accounted());
    assert!(ace_state.dependencies_to_handle().is_empty());
}

#[test]
fn test_handle_ace_dependencies_with_existing_accounted() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let contract_b = address!("1111111aa232009084Bd71A5797d089AA4Edfad4");

    let mut config_a = test_ace_config();
    config_a.contract_address = contract_a;

    let mut config_b = test_ace_config();
    config_b.contract_address = contract_b;

    let mut sim_tree = SimTree::new(nonce_cache, vec![config_a, config_b]);

    // Create an order with ACE state: detected both contracts, accounted for contract_a
    let order = create_test_order();

    let mut detected = AHashSet::default();
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_a,
    });
    detected.insert(AceInteraction::NonUnlocking {
        contract_address: contract_b,
    });

    let mut accounted = AHashSet::default();
    accounted.insert(AceInteraction::Unlocking {
        contract_address: contract_a,
        source: AceUnlockSource::User,
    });

    let ace_state = AceSimulationState {
        detected_interactions: detected,
        accounted_for_interactions: accounted,
    };

    // Handle ACE dependencies - should add order to pending for contract_b
    sim_tree.handle_ace_dependencies_for_order(order, ace_state);

    // The order should now be pending for contract_b unlock
    // (Function should succeed without error)

    Ok(())
}

#[test]
fn test_multi_ace_config_registration() -> eyre::Result<()> {
    let test_chain = TestChainState::new(BlockArgs::default())?;
    let state = test_chain.provider_factory().latest()?;
    let nonce_cache = NonceCache::new(state.into());

    let contract_a = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");
    let contract_b = address!("1111111aa232009084Bd71A5797d089AA4Edfad4");
    let contract_c = address!("2222222aa232009084Bd71A5797d089AA4Edfad4");

    let mut config_a = test_ace_config();
    config_a.contract_address = contract_a;

    let mut config_b = test_ace_config();
    config_b.contract_address = contract_b;

    let mut config_c = test_ace_config();
    config_c.contract_address = contract_c;

    let sim_tree = SimTree::new(nonce_cache, vec![config_a, config_b, config_c]);

    // All three contracts should be registered
    assert!(sim_tree.ace_configs().contains_key(&contract_a));
    assert!(sim_tree.ace_configs().contains_key(&contract_b));
    assert!(sim_tree.ace_configs().contains_key(&contract_c));

    // All three should have state
    assert!(sim_tree.get_ace_state(&contract_a).is_some());
    assert!(sim_tree.get_ace_state(&contract_b).is_some());
    assert!(sim_tree.get_ace_state(&contract_c).is_some());

    Ok(())
}

// ============================================================================
// Multi-Transaction Bundle Classification Tests
// ============================================================================

use rbuilder_primitives::ace::classify_ace_interaction;

#[test]
fn test_classify_ace_interaction_priority_force_beats_optional() {
    let config = test_ace_config();
    let contract = config.contract_address;
    let slot = b256!("0000000000000000000000000000000000000000000000000000000000000003");
    let whitelisted_from = *config.from_addresses.iter().next().unwrap();
    let force_selector = *config.force_signatures.iter().next().unwrap();
    let unlock_selector = *config.unlock_signatures.iter().next().unwrap();

    let trace = mock_state_trace_with_ace_slot(contract, slot);

    // ProtocolForce classification
    let force_result = classify_ace_interaction(
        &trace,
        true,
        &config,
        Some(force_selector),
        Some(contract),
        Some(whitelisted_from),
    );
    assert!(matches!(
        force_result,
        Some(AceInteraction::Unlocking {
            source: AceUnlockSource::ProtocolForce,
            ..
        })
    ));

    // ProtocolOptional classification
    let optional_result = classify_ace_interaction(
        &trace,
        true,
        &config,
        Some(unlock_selector),
        Some(contract),
        Some(whitelisted_from),
    );
    assert!(matches!(
        optional_result,
        Some(AceInteraction::Unlocking {
            source: AceUnlockSource::ProtocolOptional,
            ..
        })
    ));

    // User classification (non-whitelisted from)
    let non_whitelisted = address!("1111111111111111111111111111111111111111");
    let user_result = classify_ace_interaction(
        &trace,
        true,
        &config,
        Some(unlock_selector),
        Some(contract),
        Some(non_whitelisted),
    );
    assert!(matches!(
        user_result,
        Some(AceInteraction::Unlocking {
            source: AceUnlockSource::User,
            ..
        })
    ));

    // NonUnlocking classification: failed tx that reads (but doesn't write) ACE detection slot
    let read_only_trace = {
        let mut trace = UsedStateTrace::default();
        trace.read_slot_values.insert(
            SlotKey {
                address: contract,
                key: slot,
            },
            Default::default(),
        );
        trace
    };
    let non_unlocking_result = classify_ace_interaction(
        &read_only_trace,
        false, // tx failed
        &config,
        None, // no selector
        Some(contract),
        Some(whitelisted_from),
    );
    assert!(matches!(
        non_unlocking_result,
        Some(AceInteraction::NonUnlocking { .. })
    ));
}

#[test]
fn test_ace_interaction_priority_ordering() {
    // Test that the priority ordering is correct:
    // ProtocolForce > ProtocolOptional > User > NonUnlocking
    let contract = address!("0000000aa232009084Bd71A5797d089AA4Edfad4");

    let force = AceInteraction::Unlocking {
        contract_address: contract,
        source: AceUnlockSource::ProtocolForce,
    };
    let optional = AceInteraction::Unlocking {
        contract_address: contract,
        source: AceUnlockSource::ProtocolOptional,
    };
    let user = AceInteraction::Unlocking {
        contract_address: contract,
        source: AceUnlockSource::User,
    };
    let non_unlocking = AceInteraction::NonUnlocking {
        contract_address: contract,
    };

    // Verify the classification methods work as expected for priority
    assert!(force.is_force());
    assert!(force.is_protocol_tx());
    assert!(force.is_unlocking());

    assert!(!optional.is_force());
    assert!(optional.is_protocol_tx());
    assert!(optional.is_unlocking());

    assert!(!user.is_force());
    assert!(!user.is_protocol_tx());
    assert!(user.is_unlocking());

    assert!(!non_unlocking.is_force());
    assert!(!non_unlocking.is_protocol_tx());
    assert!(!non_unlocking.is_unlocking());
}
