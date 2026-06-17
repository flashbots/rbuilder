pub mod setup;

use alloy_primitives::{Address, Bytes, U256};
use reth_primitives_traits::Bytecode;
use std::collections::{HashMap, HashSet};

use crate::{
    building::{
        builders::BuiltBlockId, testing::bundle_tests::setup::NonceValue, BuiltBlockTrace,
        BundleErr, ExecutionResult, OrderErr, TransactionErr,
    },
    utils::{constants::BASE_TX_GAS, int_percentage},
};
use rbuilder_primitives::{Bundle, BundleRefund, Order, TxRevertBehavior};

use self::setup::TestSetup;

use super::test_chain_state::{BlockArgs, NamedAddr};

pub const CURR_NONCE: NonceValue = NonceValue::Relative(0);

#[macro_export]
macro_rules! commit_order_err_matches {
    ($expression:expr, $pattern:pat $(if $guard:expr)? $(,)?) => {
        $expression.commit_order_err_check(|err| assert!(matches!(err, $pattern)));
    };
}

#[test]
fn test_blocklist() -> eyre::Result<()> {
    // TODO: cases that are caught by geth access list tracer. they are handled by reth access list tracer but
    // we can add them here explicitly to be 100% sure.
    // Other case to test - we send to blocklisted address in the internal tx but revert.
    // vm.EXTCODECOPY || op == vm.EXTCODEHASH || op == vm.EXTCODESIZE || op == vm.BALANCE || op == vm.SELFDESTRUCT
    // vm.DELEGATECALL || op == vm.CALL || op == vm.STATICCALL || op == vm.CALLCODE

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default())?;

    // send to the blocked address
    test_setup.begin_mempool_tx_order();
    test_setup.add_dummy_tx(
        NamedAddr::User(0),
        NamedAddr::BlockedAddress,
        0,
        TxRevertBehavior::AllowedIncluded,
    )?;
    commit_order_err_matches!(test_setup, OrderErr::Transaction(TransactionErr::Blocklist));

    // send from the blocked address
    test_setup.begin_mempool_tx_order();
    test_setup.add_dummy_tx(
        NamedAddr::BlockedAddress,
        NamedAddr::User(0),
        0,
        TxRevertBehavior::AllowedIncluded,
    )?;
    commit_order_err_matches!(test_setup, OrderErr::Transaction(TransactionErr::Blocklist));

    // send to the blocked address using proxy contract
    test_setup.begin_mempool_tx_order();
    test_setup.add_mev_test_send_to_tx(
        NamedAddr::User(0),
        NamedAddr::BlockedAddress,
        1,
        TxRevertBehavior::AllowedIncluded,
    )?;
    commit_order_err_matches!(test_setup, OrderErr::Transaction(TransactionErr::Blocklist));

    // send to the blocked address using proxy contract with value 0
    test_setup.begin_mempool_tx_order();
    test_setup.add_mev_test_send_to_tx(
        NamedAddr::User(0),
        NamedAddr::BlockedAddress,
        0,
        TxRevertBehavior::AllowedIncluded,
    )?;
    commit_order_err_matches!(test_setup, OrderErr::Transaction(TransactionErr::Blocklist));

    Ok(())
}

#[test]
fn test_target_block() -> eyre::Result<()> {
    const BUILT_BLOCK_NUMBER: u64 = BlockArgs::MIN_BLOCK_NUMBER;
    const NEXT_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER + 1;
    const PREV_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER - 1;
    {
        let mut test_setup =
            TestSetup::gen_test_setup(BlockArgs::default().with_number(BUILT_BLOCK_NUMBER))?;
        test_setup.begin_bundle_order(BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        test_setup.commit_order_ok();

        test_setup.begin_bundle_order(NEXT_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        commit_order_err_matches!(
            test_setup,
            OrderErr::Bundle(BundleErr::TargetBlockIncorrect {
                block: BUILT_BLOCK_NUMBER,
                target_block: NEXT_BUILT_BLOCK_NUMBER,
                target_max_block: NEXT_BUILT_BLOCK_NUMBER
            })
        );

        test_setup.begin_bundle_order(PREV_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        commit_order_err_matches!(
            test_setup,
            OrderErr::Bundle(BundleErr::TargetBlockIncorrect {
                block: BUILT_BLOCK_NUMBER,
                target_block: PREV_BUILT_BLOCK_NUMBER,
                target_max_block: PREV_BUILT_BLOCK_NUMBER
            })
        );
    }

    Ok(())
}

#[test]
fn test_bundle_timestamp() -> eyre::Result<()> {
    let block_number = BlockArgs::MIN_BLOCK_NUMBER;
    {
        let block_args = BlockArgs::default().with_number(block_number);
        let base_ts = block_args.timestamp;
        // we cant't use 1000 since it's before Cancun, we must use the default (which we know executes ok) + 1000
        let block_args = block_args.with_timestamp(base_ts + 1000);
        let mut test_setup = TestSetup::gen_test_setup(block_args)?;

        let adjust_ts = |ts: Option<i32>| ts.map(|delta| delta as u64 + base_ts);
        let ok_timestamp_params = vec![
            (Some(1), Some(5000)),
            (None, Some(5000)),
            (Some(1), None),
            (Some(1000), Some(1000)),
            (None, Some(1000)),
            (Some(1000), None),
        ];

        for (min_ts, max_ts) in ok_timestamp_params {
            test_setup.begin_bundle_order(block_number);
            test_setup.set_bundle_timestamp(adjust_ts(min_ts), adjust_ts(max_ts));
            test_setup.add_dummy_tx_0_1_no_rev()?;
            test_setup.commit_order_ok();
        }

        let bad_timestamps = vec![(None, Some(999)), (Some(1001), None)];

        for (min_ts, max_ts) in bad_timestamps {
            test_setup.begin_bundle_order(block_number);
            test_setup.set_bundle_timestamp(adjust_ts(min_ts), adjust_ts(max_ts));
            test_setup.add_dummy_tx_0_1_no_rev()?;
            test_setup.commit_order_err_check_text("incorrect timestamp");
        }
    }
    Ok(())
}
#[test]
fn bundle_revert_tests() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;

    let mut current_slot_value = 0;
    // this bundle does not revert
    test_setup.begin_bundle_order(target_block);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    current_slot_value += 1;
    test_setup.commit_order_ok();

    // this bundle has incorrect nonce
    test_setup.begin_bundle_order(target_block);
    test_setup
        .add_mev_test_increment_value_tx_no_rev(NonceValue::Fixed(1000), current_slot_value)?;

    test_setup.commit_order_err_check_text("NonceTooHigh");

    // this bundle has tx that revert
    test_setup.begin_bundle_order(target_block);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value + 1)?;
    test_setup.commit_order_err_check_text("transaction reverted");

    // this bundle has 2 txs one ok other reverts
    test_setup.begin_bundle_order(target_block);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    test_setup.add_mev_test_increment_value_tx_no_rev(
        NonceValue::Relative(1),
        current_slot_value + 100,
    )?;
    test_setup.commit_order_err_check_text("transaction reverted");

    // this bundle has 2 txs one ok other reverts but its optional
    test_setup.begin_bundle_order(target_block);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    current_slot_value += 1;
    test_setup.add_mev_test_increment_value_tx(
        NonceValue::Relative(1),
        TxRevertBehavior::AllowedIncluded,
        current_slot_value + 100,
    )?;
    let result = test_setup.commit_order_ok();
    assert_eq!(result.tx_infos.len(), 2);
    assert!(result.tx_infos[0].receipt.success);
    assert!(!result.tx_infos[1].receipt.success);

    // this bundle has 2 txs one ok other has incorrect nonce
    test_setup.begin_bundle_order(target_block);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    current_slot_value += 1;
    test_setup.add_mev_test_increment_value_tx(
        NonceValue::Fixed(1000),
        TxRevertBehavior::AllowedIncluded,
        current_slot_value,
    )?;
    let result = test_setup.commit_order_ok();
    assert_eq!(result.tx_infos.len(), 1);
    assert!(result.tx_infos[0].receipt.success);

    Ok(())
}
/// Test combined refunds
#[test]
fn test_bundle_combined_refunds() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;
    let recipient = NamedAddr::User(2);
    let recipient_address = test_setup.named_address(recipient)?;
    let recipient_balance_before = test_setup.balance(recipient)?;

    let commit_refund_order =
        |setup: &mut TestSetup, recipient: Address| -> eyre::Result<ExecutionResult> {
            setup.begin_bundle_order(target_block);
            setup.add_dummy_tx_0_1_no_rev()?;
            let profit_tx_hash = setup.add_send_to_coinbase_tx(NamedAddr::User(1), profit)?;
            setup.set_bundle_refund(BundleRefund {
                recipient,
                percent: refund_percent,
                tx_hash: profit_tx_hash,
                delayed: false,
            });
            Ok(setup.commit_order_ok())
        };

    let result = commit_refund_order(&mut test_setup, recipient_address).unwrap();
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(test_setup.balance(recipient)?, recipient_balance_before);
    assert_eq!(
        test_setup.partial_block().combined_refunds,
        HashMap::from_iter([(
            recipient_address,
            U256::from(refundable_value - BASE_TX_GAS)
        )])
    );

    let result = commit_refund_order(&mut test_setup, recipient_address).unwrap();
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(test_setup.balance(recipient)?, recipient_balance_before);
    assert_eq!(
        test_setup.partial_block().combined_refunds,
        HashMap::from_iter([(
            recipient_address,
            U256::from(refundable_value * 2 - BASE_TX_GAS)
        )])
    );

    let second_recipient = NamedAddr::User(3);
    let second_recipient_address = test_setup.named_address(second_recipient)?;
    let second_recipient_balance_before = test_setup.balance(second_recipient)?;

    let result = commit_refund_order(&mut test_setup, second_recipient_address).unwrap();
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(
        test_setup.balance(second_recipient)?,
        second_recipient_balance_before
    );
    assert_eq!(
        test_setup.partial_block().combined_refunds,
        HashMap::from_iter([
            (
                recipient_address,
                U256::from(refundable_value * 2 - BASE_TX_GAS)
            ),
            (
                second_recipient_address,
                U256::from(refundable_value - BASE_TX_GAS)
            )
        ])
    );

    Ok(())
}

/// Test delayed refunds
#[test]
fn test_bundle_delayed_refunds() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);
    let coinbase_profit = profit - refundable_value;

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;
    let recipient = NamedAddr::User(2);
    let recipient_address = test_setup.named_address(recipient)?;
    let recipient_balance_before = test_setup.balance(recipient)?;

    let commit_refund_order =
        |setup: &mut TestSetup, recipient: Address| -> eyre::Result<ExecutionResult> {
            setup.begin_bundle_order(target_block);
            setup.add_dummy_tx_0_1_no_rev()?;
            let profit_tx_hash = setup.add_send_to_coinbase_tx(NamedAddr::User(1), profit)?;
            setup.set_bundle_refund(BundleRefund {
                recipient,
                percent: refund_percent,
                tx_hash: profit_tx_hash,
                delayed: true,
            });
            Ok(setup.commit_order_ok())
        };

    let result = commit_refund_order(&mut test_setup, recipient_address).unwrap();
    let partial_block = test_setup.partial_block();
    assert_eq!(test_setup.balance(recipient)?, recipient_balance_before);
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(result.coinbase_profit, U256::from(coinbase_profit));
    assert!(partial_block.combined_refunds.is_empty());
    assert_eq!(partial_block.delayed_refund, U256::from(refundable_value));
    assert_eq!(partial_block.coinbase_profit, U256::from(coinbase_profit));

    let result = commit_refund_order(&mut test_setup, recipient_address).unwrap();
    let partial_block = test_setup.partial_block();
    assert_eq!(test_setup.balance(recipient)?, recipient_balance_before);
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(result.coinbase_profit, U256::from(coinbase_profit));
    assert!(partial_block.combined_refunds.is_empty());
    assert_eq!(
        partial_block.delayed_refund,
        U256::from(refundable_value * 2)
    );
    assert_eq!(
        partial_block.coinbase_profit,
        U256::from(coinbase_profit * 2)
    );

    let second_recipient = NamedAddr::User(3);
    let second_recipient_address = test_setup.named_address(second_recipient)?;
    let second_recipient_balance_before = test_setup.balance(second_recipient)?;

    let result = commit_refund_order(&mut test_setup, second_recipient_address).unwrap();
    let partial_block = test_setup.partial_block();
    assert_eq!(
        test_setup.balance(second_recipient)?,
        second_recipient_balance_before
    );
    assert!(result.paid_kickbacks.is_empty());
    assert_eq!(result.coinbase_profit, U256::from(coinbase_profit));
    assert!(partial_block.combined_refunds.is_empty());
    assert_eq!(
        partial_block.delayed_refund,
        U256::from(refundable_value * 3)
    );
    assert_eq!(
        partial_block.coinbase_profit,
        U256::from(coinbase_profit * 3)
    );

    Ok(())
}

/// Test immediate refunds to contract recipients
#[test]
fn test_bundle_contract_refunds() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;
    let recipient_named_address = NamedAddr::User(2);
    let recipient_contract_address = test_setup.named_address(recipient_named_address)?;
    test_setup
        .chain_state_mut()
        .upsert_contract(
            recipient_contract_address,
            Bytecode::new_raw(Bytes::from([0x38 /* CODESIZE */])),
        )
        .unwrap();

    let recipient_balance_before = test_setup.balance(recipient_named_address)?;
    test_setup.begin_bundle_order(target_block);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    let profit_tx_hash = test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), profit)?;
    test_setup.set_bundle_refund(BundleRefund {
        percent: refund_percent,
        recipient: recipient_contract_address,
        tx_hash: profit_tx_hash,
        delayed: false,
    });
    let result = test_setup.commit_order_ok();
    let recipient_balance_after = test_setup.balance(recipient_named_address)?;
    let expected_refund = refundable_value - BASE_TX_GAS - 2 /* 0x38 CODESIZE cost */;
    assert_eq!(
        recipient_balance_after - recipient_balance_before,
        expected_refund as i128
    );
    assert_eq!(
        result.paid_kickbacks,
        vec![(recipient_contract_address, U256::from(expected_refund))]
    );
    Ok(())
}

#[test]
fn test_bundle_ok_inner_tx_profits() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;
    let profits = [100_000u64, 200_000u64, 10u64];
    let mut tx_hashes = Vec::default();
    test_setup.begin_bundle_order(target_block);
    for (index, profit) in profits.iter().enumerate() {
        tx_hashes.push(test_setup.add_send_to_coinbase_tx(NamedAddr::User(index), *profit)?);
    }
    let result = test_setup.commit_order_ok();
    let executed_profits = result
        .tx_infos
        .iter()
        .map(|info| u64::try_from(info.coinbase_profit).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(profits, executed_profits.as_slice());
    Ok(())
}

#[test]
fn test_bundle_consistency_check() -> eyre::Result<()> {
    use std::sync::Arc;
    let block_number = BlockArgs::MIN_BLOCK_NUMBER;

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(block_number))?;

    let blocklist = HashSet::default();
    // check revertible tx detection
    {
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO, 0);

        // send to the blocked address
        test_setup.begin_bundle_order(block_number);
        // this tx will revert
        test_setup.add_mev_test_increment_value_tx(
            CURR_NONCE,
            TxRevertBehavior::AllowedIncluded,
            1,
        )?;

        let mut res = test_setup.commit_order_ok();

        // break bundle by removing revertible tx hashes
        if let Order::Bundle(Bundle {
            reverting_tx_hashes,
            ..
        }) = Arc::make_mut(&mut res.order)
        {
            reverting_tx_hashes.clear();
        } else {
            unreachable!()
        }
        built_block_trace.add_included_order(res);
        let err = built_block_trace
            .verify_bundle_consistency(&blocklist)
            .expect_err("Expected error");
        assert!(err.to_string().contains("Bundle tx reverted"));
    }

    // check commit of blocklisted tx from
    {
        let blocklist = vec![test_setup.named_address(NamedAddr::User(0))?]
            .into_iter()
            .collect();
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO, 0);

        test_setup.begin_bundle_order(block_number);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        let res = test_setup.commit_order_ok();
        built_block_trace.add_included_order(res);

        let err = built_block_trace
            .verify_bundle_consistency(&blocklist)
            .expect_err("Expected error");
        assert!(err.to_string().contains("blocked address"));
    }

    // check commit of blocklisted tx to
    {
        let blocklist = vec![test_setup.named_address(NamedAddr::User(1))?]
            .into_iter()
            .collect();
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO, 0);

        test_setup.begin_bundle_order(block_number);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        let res = test_setup.commit_order_ok();
        built_block_trace.add_included_order(res);

        let err = built_block_trace
            .verify_bundle_consistency(&blocklist)
            .expect_err("Expected error");
        assert!(err.to_string().contains("blocked address"));
    }

    Ok(())
}

/// Tests allow_tx_skip behavior with two bundles sharing a reverting tx.
/// Bundle 1: reverting_tx_1 + backrun_1 — both execute successfully.
/// Bundle 2: reverting_tx_1 (stale nonce, skipped) + reverting_tx_2 + backrun_2 — only last two execute.
#[test]
fn test_bundle_allow_tx_skip() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;

    let reverting_tx_1 = test_setup.create_dummy_tx(NamedAddr::User(0), NamedAddr::Dummy, 0)?;
    // Bundle 1: reverting_tx_1 (User(0), reverting) + backrun_1 (User(1))
    test_setup.begin_bundle_order(target_block);
    test_setup.add_external_tx(reverting_tx_1.clone(), TxRevertBehavior::AllowedIncluded)?;
    test_setup.add_null_tx(NamedAddr::User(1), TxRevertBehavior::NotAllowed)?;
    let result = test_setup.commit_order_ok();
    assert_eq!(result.tx_infos.len(), 2);
    assert!(result.tx_infos[0].receipt.success);
    assert!(result.tx_infos[1].receipt.success);

    // Bundle 2: SAME reverting_tx_1 as in bundle 1.
    //         + reverting_tx_2 (User(2), reverting)
    //         + backrun_2 (User(3))
    test_setup.begin_bundle_order(target_block);
    // reverting_tx_1 with stale nonce — will fail with NonceTooHigh and be skipped
    // because allow_tx_skip=true and it's in reverting_tx_hashes
    test_setup.add_external_tx(reverting_tx_1, TxRevertBehavior::AllowedIncluded)?;
    test_setup.add_null_tx(NamedAddr::User(2), TxRevertBehavior::AllowedIncluded)?;
    test_setup.add_null_tx(NamedAddr::User(3), TxRevertBehavior::NotAllowed)?;
    let result = test_setup.commit_order_ok();
    // reverting_tx_1 was skipped, only reverting_tx_2 + backrun_2 landed
    assert_eq!(result.tx_infos.len(), 2);
    assert!(result.tx_infos[0].receipt.success);
    assert!(result.tx_infos[1].receipt.success);

    Ok(())
}

#[test]
///Checks TxRevertBehavior::AllowedInclude/AllowedExcluded by checking the consumed gas.
fn bundle_revert_modes_tests() -> eyre::Result<()> {
    let target_block = BlockArgs::MIN_BLOCK_NUMBER;
    // 2 users to avoid caring about nonces
    let tx_sender0 = NamedAddr::User(0);
    let tx_sender1 = NamedAddr::User(1);
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().with_number(target_block))?;

    // Single revert tx AllowedExcluded -> NO GAS
    test_setup.begin_bundle_order(target_block);
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedExcluded)?;
    test_setup.commit_order_err_check(|err| {
        assert!(matches!(err, OrderErr::Bundle(BundleErr::EmptyBundle)));
    });

    // Measure simple tx
    test_setup.begin_bundle_order(target_block);
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedIncluded)?;
    let res = test_setup.commit_order_ok();
    let reverting_gas = res.space_used.gas;

    // Measure reverting tx
    test_setup.begin_bundle_order(target_block);
    test_setup.add_send_to_coinbase_tx(tx_sender0, 0)?;
    let res = test_setup.commit_order_ok();
    let send_gas = res.space_used.gas;

    // send + rev on AllowedIncluded pay both gases
    test_setup.begin_bundle_order(target_block);
    test_setup.add_send_to_coinbase_tx(tx_sender1, 0)?;
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedIncluded)?;
    let res = test_setup.commit_order_ok();
    assert_eq!(res.space_used.gas, send_gas + reverting_gas);

    // send + rev on AllowedExcluded pay send
    test_setup.begin_bundle_order(target_block);
    test_setup.add_send_to_coinbase_tx(tx_sender0, 0)?;
    test_setup.add_revert(tx_sender1, TxRevertBehavior::AllowedExcluded)?;
    let res = test_setup.commit_order_ok();
    assert_eq!(res.space_used.gas, send_gas);

    Ok(())
}
