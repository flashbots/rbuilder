pub mod setup;

use alloy_primitives::{Address, Bytes, B256, U256};
use itertools::Itertools;
use reth_primitives::Bytecode;
use std::collections::{HashMap, HashSet};

use crate::{
    building::{
        builders::BuiltBlockId, testing::bundle_tests::setup::NonceValue, BuiltBlockTrace,
        BundleErr, ExecutionResult, OrderErr, TransactionErr,
    },
    utils::{constants::BASE_TX_GAS, int_percentage},
};
use rbuilder_primitives::{
    Bundle, BundleRefund, Order, OrderId, Refund, RefundConfig, TxRevertBehavior,
};

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
    const BUILT_BLOCK_NUMBER: u64 = 11;
    const NEXT_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER + 1;
    const PREV_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER - 1;
    const NEXT_NEXT_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER + 2;
    const PREV_PREV_BUILT_BLOCK_NUMBER: u64 = BUILT_BLOCK_NUMBER - 2;

    {
        let mut test_setup =
            TestSetup::gen_test_setup(BlockArgs::default().number(BUILT_BLOCK_NUMBER))?;
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

    {
        let mut test_setup =
            TestSetup::gen_test_setup(BlockArgs::default().number(BUILT_BLOCK_NUMBER))?;
        test_setup.begin_share_bundle_order(PREV_BUILT_BLOCK_NUMBER, NEXT_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        test_setup.commit_order_ok();

        test_setup.begin_share_bundle_order(BUILT_BLOCK_NUMBER, BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        test_setup.commit_order_ok();

        test_setup.begin_share_bundle_order(BUILT_BLOCK_NUMBER, NEXT_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        test_setup.commit_order_ok();

        test_setup.begin_share_bundle_order(PREV_PREV_BUILT_BLOCK_NUMBER, PREV_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        commit_order_err_matches!(
            test_setup,
            OrderErr::Bundle(BundleErr::TargetBlockIncorrect {
                block: BUILT_BLOCK_NUMBER,
                target_block: PREV_PREV_BUILT_BLOCK_NUMBER,
                target_max_block: PREV_BUILT_BLOCK_NUMBER
            })
        );

        test_setup.begin_share_bundle_order(NEXT_BUILT_BLOCK_NUMBER, NEXT_NEXT_BUILT_BLOCK_NUMBER);
        test_setup.add_dummy_tx_0_1_no_rev()?;
        commit_order_err_matches!(
            test_setup,
            OrderErr::Bundle(BundleErr::TargetBlockIncorrect {
                block: BUILT_BLOCK_NUMBER,
                target_block: NEXT_BUILT_BLOCK_NUMBER,
                target_max_block: NEXT_NEXT_BUILT_BLOCK_NUMBER
            })
        );
    }

    Ok(())
}

#[test]
fn test_bundle_timestamp() -> eyre::Result<()> {
    {
        let block_args = BlockArgs::default().number(11);
        let base_ts = block_args.timestamp;
        // we cant't use 1000 since it's before Cancun, we must use the default (which we know executes ok) + 1000
        let block_args = block_args.timestamp(base_ts + 1000);
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
            test_setup.begin_bundle_order(11);
            test_setup.set_bundle_timestamp(adjust_ts(min_ts), adjust_ts(max_ts));
            test_setup.add_dummy_tx_0_1_no_rev()?;
            test_setup.commit_order_ok();
        }

        let bad_timestamps = vec![(None, Some(999)), (Some(1001), None)];

        for (min_ts, max_ts) in bad_timestamps {
            test_setup.begin_bundle_order(11);
            test_setup.set_bundle_timestamp(adjust_ts(min_ts), adjust_ts(max_ts));
            test_setup.add_dummy_tx_0_1_no_rev()?;
            test_setup.commit_order_err_check_text("incorrect timestamp");
        }
    }
    Ok(())
}

fn bundle_revert_tests(
    test_setup: &mut TestSetup,
    target_block: u64,
    share_bundle: bool,
) -> eyre::Result<()> {
    let mut current_slot_value = 0;
    let begin_bundle = |test_setup: &mut TestSetup| {
        if share_bundle {
            test_setup.begin_share_bundle_order(target_block, target_block);
        } else {
            test_setup.begin_bundle_order(target_block);
        }
    };
    // this bundle does not revert
    begin_bundle(test_setup);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    current_slot_value += 1;
    test_setup.commit_order_ok();

    // this bundle has incorrect nonce
    begin_bundle(test_setup);
    test_setup
        .add_mev_test_increment_value_tx_no_rev(NonceValue::Fixed(1000), current_slot_value)?;

    test_setup.commit_order_err_check_text("NonceTooHigh");

    // this bundle has tx that revert
    begin_bundle(test_setup);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value + 1)?;
    test_setup.commit_order_err_check_text("transaction reverted");

    // this bundle has 2 txs one ok other reverts
    begin_bundle(test_setup);
    test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
    test_setup.add_mev_test_increment_value_tx_no_rev(
        NonceValue::Relative(1),
        current_slot_value + 100,
    )?;
    test_setup.commit_order_err_check_text("transaction reverted");

    // this bundle has 2 txs one ok other reverts but its optional
    begin_bundle(test_setup);
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
    begin_bundle(test_setup);
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

    // for share bundle also try nested bundles
    if share_bundle {
        // this bundle with 2 ok txs
        test_setup.begin_share_bundle_order(target_block, target_block);
        test_setup.start_inner_bundle(false);
        test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
        current_slot_value += 1;
        test_setup.finish_inner_bundle();
        test_setup
            .add_mev_test_increment_value_tx_no_rev(NonceValue::Relative(1), current_slot_value)?;
        current_slot_value += 1;
        test_setup.commit_order_ok();

        // this bundle with 1 inner tx that fails
        test_setup.begin_share_bundle_order(target_block, target_block);
        test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
        test_setup.start_inner_bundle(false);
        test_setup.add_mev_test_increment_value_tx_no_rev(
            NonceValue::Relative(1),
            current_slot_value + 100,
        )?;

        test_setup.finish_inner_bundle();
        test_setup.commit_order_err_check_text("Transaction reverted");

        // this bundle with 1 optional inner tx that fails
        test_setup.begin_share_bundle_order(target_block, target_block);
        test_setup.add_mev_test_increment_value_tx_no_rev(CURR_NONCE, current_slot_value)?;
        test_setup.start_inner_bundle(false);

        test_setup.add_mev_test_increment_value_tx(
            NonceValue::Relative(1),
            TxRevertBehavior::AllowedIncluded,
            current_slot_value + 100,
        )?;
        test_setup.finish_inner_bundle();
        test_setup.commit_order_ok();
    }
    Ok(())
}

#[test]
fn test_bundle_revert() -> eyre::Result<()> {
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;

    bundle_revert_tests(&mut test_setup, target_block, false)?;

    Ok(())
}

#[test]
fn test_share_bundle_revert() -> eyre::Result<()> {
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;

    bundle_revert_tests(&mut test_setup, target_block, true)?;

    Ok(())
}

/// Test combined refunds
#[test]
fn test_bundle_combined_refunds() -> eyre::Result<()> {
    let target_block = 11;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
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
    let target_block = 11;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);
    let coinbase_profit = profit - refundable_value;

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
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
    let target_block = 11;
    let profit: u64 = 100_000;
    let refund_percent: u8 = 90;
    let refundable_value = int_percentage(profit, refund_percent as usize);

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
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
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
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
fn test_mev_share_ok_refunds() -> eyre::Result<()> {
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;

    test_setup.begin_share_bundle_order(11, 11);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 100_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 90,
    }]);
    let result = test_setup.commit_order_ok();
    assert_eq!(
        result.paid_kickbacks,
        vec![(
            test_setup.named_address(NamedAddr::User(0))?,
            U256::from(90_000 - 21_000)
        )]
    );

    test_setup.begin_share_bundle_order(11, 11);
    test_setup.start_inner_bundle(false);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.finish_inner_bundle();
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 100_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 90,
    }]);
    let result = test_setup.commit_order_ok();
    assert_eq!(
        result.paid_kickbacks,
        vec![(
            test_setup.named_address(NamedAddr::User(0))?,
            U256::from(90_000 - 21_000)
        )]
    );

    test_setup.begin_share_bundle_order(11, 11);
    test_setup.start_inner_bundle(false);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.set_inner_bundle_refund_config(vec![RefundConfig {
        address: test_setup.named_address(NamedAddr::User(3))?,
        percent: 100,
    }]);
    test_setup.finish_inner_bundle();
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 100_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 90,
    }]);
    let result = test_setup.commit_order_ok();
    assert_eq!(
        result.paid_kickbacks,
        vec![(
            test_setup.named_address(NamedAddr::User(3))?,
            U256::from(90_000 - 21_000)
        )]
    );

    test_setup.begin_share_bundle_order(11, 11);
    test_setup.start_inner_bundle(false);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.set_inner_bundle_refund_config(vec![
        RefundConfig {
            address: test_setup.named_address(NamedAddr::User(1))?,
            percent: 10,
        },
        RefundConfig {
            address: test_setup.named_address(NamedAddr::User(2))?,
            percent: 20,
        },
        RefundConfig {
            address: test_setup.named_address(NamedAddr::User(3))?,
            percent: 70,
        },
    ]);
    test_setup.finish_inner_bundle();
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 500_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 90,
    }]);
    let result = test_setup.commit_order_ok();
    let got_kickbacks: Vec<_> = result
        .paid_kickbacks
        .into_iter()
        .sorted_by_key(|(a, _)| *a)
        .collect();
    let expected_kickbacks: Vec<_> = vec![
        (
            test_setup.named_address(NamedAddr::User(1))?,
            U256::from(500_000 * 90 / 100 * 10 / 100 - 21_000),
        ),
        (
            test_setup.named_address(NamedAddr::User(2))?,
            U256::from(500_000 * 90 / 100 * 20 / 100 - 21_000),
        ),
        (
            test_setup.named_address(NamedAddr::User(3))?,
            U256::from(500_000 * 90 / 100 * 70 / 100 - 21_000),
        ),
    ]
    .into_iter()
    .sorted_by_key(|(a, _)| *a)
    .collect();
    assert_eq!(got_kickbacks, expected_kickbacks);

    // test refund config values above 100 percent
    // in this example refund set to 50% but refund config to 200%
    // that is equivalent to having refund set to 100% - all profit to the user
    test_setup.begin_share_bundle_order(11, 11);
    test_setup.start_inner_bundle(false);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.set_inner_bundle_refund_config(vec![RefundConfig {
        address: test_setup.named_address(NamedAddr::User(3))?,
        percent: 200,
    }]);
    test_setup.finish_inner_bundle();
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 42_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 50,
    }]);
    let result = test_setup.commit_order_ok();
    assert_eq!(
        result.paid_kickbacks,
        vec![(
            test_setup.named_address(NamedAddr::User(3))?,
            U256::from(42_000 - 21_000)
        )]
    );

    Ok(())
}

#[test]
fn test_mev_share_failed_refunds() -> eyre::Result<()> {
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;

    test_setup.begin_share_bundle_order(11, 11);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 21_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 90,
    }]);
    test_setup.commit_order_err_check(|err| {
        assert!(matches!(
            err,
            OrderErr::Bundle(BundleErr::NotEnoughRefundForGas {
                to: _,
                refundable_value: _,
                needed_value: _,
            })
        ))
    });

    // this bundle tries to go into the builder balance by having really high refund config percent
    test_setup.begin_share_bundle_order(11, 11);
    test_setup.start_inner_bundle(false);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.set_inner_bundle_refund_config(vec![RefundConfig {
        address: test_setup.named_address(NamedAddr::User(3))?,
        percent: 201,
    }]);
    test_setup.finish_inner_bundle();
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 42_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 50,
    }]);
    test_setup.commit_order_err_check(|err| assert!(matches!(err, OrderErr::NegativeProfit(_))));

    // this bundle tries to go into the builder balance by having high refund percentage
    test_setup.begin_share_bundle_order(11, 11);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.add_send_to_coinbase_tx(NamedAddr::User(1), 42_000)?;
    test_setup.set_inner_bundle_refund(vec![Refund {
        body_idx: 0,
        percent: 101,
    }]);
    test_setup.commit_order_err_check(|err| assert!(matches!(err, OrderErr::NegativeProfit(_))));

    Ok(())
}

#[test]
fn test_bundle_consistency_check() -> eyre::Result<()> {
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(11))?;

    let blocklist = HashSet::default();
    // check revertible tx detection
    {
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO);

        // send to the blocked address
        test_setup.begin_bundle_order(11);
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
        }) = &mut res.order
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
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO);

        test_setup.begin_bundle_order(11);
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
        let mut built_block_trace = BuiltBlockTrace::new(BuiltBlockId::ZERO);

        test_setup.begin_bundle_order(11);
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

#[test]
///Checks TxRevertBehavior::AllowedInclude/AllowedExcluded by checking the consumed gas.
fn test_bundle_revert_modes() -> eyre::Result<()> {
    bundle_revert_modes_tests(false)?;
    bundle_revert_modes_tests(true)?;
    Ok(())
}

///Checks TxRevertBehavior::AllowedInclude/AllowedExcluded by checking the consumed gas.
fn bundle_revert_modes_tests(share_bundle: bool) -> eyre::Result<()> {
    let target_block = 11;
    // 2 users to avoid caring about nonces
    let tx_sender0 = NamedAddr::User(0);
    let tx_sender1 = NamedAddr::User(1);
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
    let begin_bundle = |test_setup: &mut TestSetup| {
        if share_bundle {
            test_setup.begin_share_bundle_order(target_block, target_block);
        } else {
            test_setup.begin_bundle_order(target_block);
        }
    };

    // Single revert tx AllowedExcluded -> NO GAS
    begin_bundle(&mut test_setup);
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedExcluded)?;
    // Bundles behave different to sbundles on empty execution
    if share_bundle {
        let res = test_setup.commit_order_ok();
        assert_eq!(res.space_used.gas, 0);
    } else {
        test_setup.commit_order_err_check(|err| {
            assert!(matches!(err, OrderErr::Bundle(BundleErr::EmptyBundle)));
        });
    }

    // Measure simple tx
    begin_bundle(&mut test_setup);
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedIncluded)?;
    let res = test_setup.commit_order_ok();
    let reverting_gas = res.space_used.gas;

    // Measure reverting tx
    begin_bundle(&mut test_setup);
    test_setup.add_send_to_coinbase_tx(tx_sender0, 0)?;
    let res = test_setup.commit_order_ok();
    let send_gas = res.space_used.gas;

    // send + rev on AllowedIncluded pay both gases
    begin_bundle(&mut test_setup);
    test_setup.add_send_to_coinbase_tx(tx_sender1, 0)?;
    test_setup.add_revert(tx_sender0, TxRevertBehavior::AllowedIncluded)?;
    let res = test_setup.commit_order_ok();
    assert_eq!(res.space_used.gas, send_gas + reverting_gas);

    // send + rev on AllowedExcluded pay send
    begin_bundle(&mut test_setup);
    test_setup.add_send_to_coinbase_tx(tx_sender0, 0)?;
    test_setup.add_revert(tx_sender1, TxRevertBehavior::AllowedExcluded)?;
    let res = test_setup.commit_order_ok();
    assert_eq!(res.space_used.gas, send_gas);

    Ok(())
}

#[test]
/// Checks that failing subbundle with can_skip does not revert the parent bundle.
fn test_subbundle_skip() -> eyre::Result<()> {
    let target_block = 11;
    // 2 users to avoid caring about nonces
    let tx_sender0 = NamedAddr::User(0);
    let tx_sender1 = NamedAddr::User(1);
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;

    // First bundle not skipable , it fails -> the whole bundle fails
    test_setup.begin_share_bundle_order(target_block, target_block);
    test_setup.start_inner_bundle(false);
    let revert_hash = test_setup.add_revert(tx_sender0, TxRevertBehavior::NotAllowed)?;
    test_setup.finish_inner_bundle();

    test_setup.start_inner_bundle(false);
    test_setup.add_send_to_coinbase_tx(tx_sender1, 0)?;
    test_setup.finish_inner_bundle();

    test_setup.commit_order_err_check(|err| {
        if let OrderErr::Bundle(BundleErr::TransactionReverted(hash)) = err {
            assert_eq!(hash, revert_hash);
        } else {
            panic!("got {err} while expecting OrderErr::Bundle(BundleErr::TransactionReverted)");
        }
    });

    // First bundle skipable , it fails -> life goes on
    test_setup.begin_share_bundle_order(target_block, target_block);
    test_setup.start_inner_bundle(true);
    test_setup.add_revert(tx_sender0, TxRevertBehavior::NotAllowed)?;
    test_setup.finish_inner_bundle();

    test_setup.start_inner_bundle(false);
    test_setup.add_send_to_coinbase_tx(tx_sender1, 0)?;
    test_setup.finish_inner_bundle();

    test_setup.commit_order_ok();
    Ok(())
}

#[test]
/// This is a real life example.
/// Some order flow providers (handled in [`ShareBundleMerger`]) sends its user transactions (A in this example) AllowedExcluded so we can chain them [A,Br1]+[A+Br2]+[A+Br3]
/// Usually, when executing normally, the second A will fail by nonce but that's ok, we will get A,Br1,Br2,Br3 and the user will have multiple paybacks!
/// Br1/Br3 execute ok, Br2 fails
fn test_mergeable_multibackrun() -> eyre::Result<()> {
    let target_block = 11;
    let a_sender = NamedAddr::User(0);
    let br1_sender = NamedAddr::User(1);
    let br2_sender = NamedAddr::User(2);
    let br3_sender = NamedAddr::User(3);
    let br1_payment = 100_000;
    let br3_payment = 200_000;
    let kickback_percentage: usize = 90;
    let expected_kickback = int_percentage(br1_payment, kickback_percentage)
        + int_percentage(br3_payment, kickback_percentage)
        - BASE_TX_GAS;

    // Func to add a subbundle [A,Br]
    let add_backrun = |test_setup: &mut TestSetup, backrunner: NamedAddr, payment: u64| {
        test_setup.start_inner_bundle(true);
        test_setup
            .add_null_tx(a_sender, TxRevertBehavior::AllowedExcluded)
            .unwrap();
        test_setup
            .add_send_to_coinbase_tx(backrunner, payment)
            .unwrap();
        test_setup.set_inner_bundle_refund(vec![Refund {
            body_idx: 0,
            percent: kickback_percentage,
        }]);
        test_setup.finish_inner_bundle();
    };

    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
    test_setup.begin_share_bundle_order(target_block, target_block);

    //[A,Br1]
    add_backrun(&mut test_setup, br1_sender, br1_payment);

    //[A,Br2 (reverts)]
    test_setup.start_inner_bundle(true);
    test_setup.add_null_tx(a_sender, TxRevertBehavior::AllowedExcluded)?;
    test_setup.add_revert(br2_sender, TxRevertBehavior::NotAllowed)?;
    test_setup.finish_inner_bundle();

    //[A,Br3]
    add_backrun(&mut test_setup, br3_sender, br3_payment);

    let result = test_setup.commit_order_ok();
    assert_eq!(
        result.paid_kickbacks,
        vec![(
            test_setup.named_address(a_sender)?,
            U256::from(expected_kickback)
        )]
    );
    Ok(())
}

#[test]
/// Test the proper propagation of original_order_id on execution
fn test_original_order_id() -> eyre::Result<()> {
    let target_block = 11;
    let mut test_setup = TestSetup::gen_test_setup(BlockArgs::default().number(target_block))?;
    let original_order_id = OrderId::ShareBundle(B256::with_last_byte(123));
    // Simple good tx with bundle order it should report the order id
    test_setup.begin_share_bundle_order(target_block, target_block);
    test_setup.start_inner_bundle(true);
    test_setup.set_inner_bundle_original_order_id(original_order_id);
    test_setup.add_dummy_tx_0_1_no_rev()?;
    test_setup.finish_inner_bundle();
    let result = test_setup.commit_order_ok();
    assert_eq!(result.original_order_ids, vec![original_order_id]);

    // Reverting tx will leave an empty execution so order id should not pass
    test_setup.begin_share_bundle_order(target_block, target_block);
    test_setup.start_inner_bundle(true);
    test_setup.set_inner_bundle_original_order_id(original_order_id);
    test_setup.add_revert(
        NamedAddr::User(0), /*don't care*/
        TxRevertBehavior::AllowedExcluded,
    )?;
    test_setup.finish_inner_bundle();
    let result = test_setup.commit_order_ok();
    assert_eq!(result.original_order_ids, Vec::new());

    Ok(())
}
