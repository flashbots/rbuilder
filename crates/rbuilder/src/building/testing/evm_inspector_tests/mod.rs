use alloy_primitives::{B256, U256};
use reth_primitives::TransactionSigned;

use crate::building::testing::{
    evm_inspector_tests::setup::TestSetup, test_chain_state::NamedAddr,
};
use rbuilder_primitives::evm_inspector::SlotKey;

pub mod setup;

#[test]
fn test_transfer() -> eyre::Result<()> {
    let test_setup = TestSetup::new()?;

    let sender = NamedAddr::User(1);
    let receiver = NamedAddr::User(2);
    let transfer_value = 10000;

    let tx = test_setup.make_transfer_tx(sender, receiver, transfer_value)?;
    let used_state_trace = test_setup.inspect_tx_without_commit(tx)?;

    // check sent_amount/received_amount
    let sender_addr = test_setup.named_address(sender)?;
    let receiver_addr = test_setup.named_address(receiver)?;
    assert_eq!(used_state_trace.received_amount.len(), 1);
    assert_eq!(
        used_state_trace.received_amount.get(&receiver_addr),
        Some(&U256::from(transfer_value))
    );
    assert_eq!(used_state_trace.sent_amount.len(), 1);
    assert_eq!(
        used_state_trace.sent_amount.get(&sender_addr),
        Some(&U256::from(transfer_value))
    );

    // check read_slot_values/written_slot_values
    let sender_nonce_slot_key = SlotKey {
        address: sender_addr,
        key: B256::ZERO,
    };
    assert_eq!(used_state_trace.read_slot_values.len(), 1);
    assert_eq!(used_state_trace.written_slot_values.len(), 1);
    let nonce_read_value = used_state_trace
        .read_slot_values
        .get(&sender_nonce_slot_key);
    let nonce_written_value = used_state_trace
        .written_slot_values
        .get(&sender_nonce_slot_key);
    assert!(nonce_read_value.is_some());
    assert!(nonce_written_value.is_some());
    let nonce_read_value: U256 = (*nonce_read_value.unwrap()).into();
    let nonce_written_value: U256 = (*nonce_written_value.unwrap()).into();
    assert_eq!(
        nonce_written_value.checked_sub(nonce_read_value),
        Some(U256::from(1))
    );

    // check created_contracts/destructed_contracts
    assert!(used_state_trace.created_contracts.is_empty());
    assert!(used_state_trace.destructed_contracts.is_empty());

    Ok(())
}

#[test]
fn test_call_contract() -> eyre::Result<()> {
    let test_setup = TestSetup::new()?;

    let tx = test_setup.make_increment_value_tx(100, 0)?;
    let used_state_trace = test_setup.inspect_tx_without_commit(tx)?;

    let test_contract_addr = test_setup.test_contract_address()?;
    let slot_key = SlotKey {
        address: test_contract_addr,
        key: B256::from(U256::from(100)),
    };
    assert_eq!(
        used_state_trace.read_slot_values.get(&slot_key),
        Some(&B256::from(U256::from(0)))
    );
    assert_eq!(
        used_state_trace.written_slot_values.get(&slot_key),
        Some(&B256::from(U256::from(1)))
    );

    Ok(())
}

#[test]
fn test_deploy_contract() -> eyre::Result<()> {
    let test_setup = TestSetup::new()?;

    let tx = test_setup.make_deploy_mev_test_tx()?;
    let used_state_trace = test_setup.inspect_tx_without_commit(tx)?;

    assert_eq!(used_state_trace.created_contracts.len(), 1);

    Ok(())
}

#[test]
fn test_read_balance() -> eyre::Result<()> {
    let test_setup = TestSetup::new()?;

    let mev_test_contract_addr = test_setup.named_address(NamedAddr::MevTest)?;
    let dummy_addr = test_setup.named_address(NamedAddr::Dummy)?;
    let tx: reth_primitives::Recovered<TransactionSigned> =
        test_setup.make_test_read_balance_tx(dummy_addr, 100)?;
    let used_state_trace = test_setup.inspect_tx_without_commit(tx)?;

    assert_eq!(used_state_trace.read_balances.len(), 2);
    assert_eq!(
        used_state_trace.read_balances.get(&dummy_addr),
        Some(&U256::from(0))
    );
    assert_eq!(
        used_state_trace.read_balances.get(&mev_test_contract_addr),
        Some(&U256::from(100))
    );

    Ok(())
}

#[test]
fn test_ephemeral_contract_destruct() -> eyre::Result<()> {
    let test_setup = TestSetup::new()?;

    let refund_addr = test_setup.named_address(NamedAddr::Dummy)?;
    let tx: reth_primitives::Recovered<TransactionSigned> =
        test_setup.make_test_ephemeral_contract_destruct_tx(refund_addr, 100)?;
    let used_state_trace = test_setup.inspect_tx_without_commit(tx)?;

    assert_eq!(used_state_trace.created_contracts.len(), 1);
    assert_eq!(used_state_trace.destructed_contracts.len(), 1);
    assert_eq!(
        used_state_trace.created_contracts[0],
        used_state_trace.destructed_contracts[0]
    );
    let ephemeral_contract_addr = used_state_trace.created_contracts[0];
    assert_eq!(
        used_state_trace
            .received_amount
            .get(&ephemeral_contract_addr),
        Some(&U256::from(100))
    );
    assert_eq!(
        used_state_trace.sent_amount.get(&ephemeral_contract_addr),
        Some(&U256::from(100))
    );
    assert_eq!(
        used_state_trace.received_amount.get(&refund_addr),
        Some(&U256::from(100))
    );

    Ok(())
}
