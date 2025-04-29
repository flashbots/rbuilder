#[cfg(all(test, feature = "integration"))]
mod tests {
    use crate::{
        integration::{op_rbuilder::OpRbuilderConfig, op_reth::OpRethConfig, IntegrationFramework},
        tester::{BlockGenerator, EngineApi},
        tx_signer::Signer,
    };
    use alloy_consensus::{Transaction, TxEip1559};
    use alloy_eips::{eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718};
    use alloy_primitives::hex;
    use alloy_provider::{ext::TxPoolApi, Identity, Provider, ProviderBuilder};
    use alloy_rpc_types_eth::BlockTransactionsKind;
    use futures_util::StreamExt;
    use op_alloy_consensus::OpTypedTransaction;
    use op_alloy_network::{Optimism, TransactionResponse};
    use std::{
        cmp::max,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio_tungstenite::connect_async;
    use uuid::Uuid;

    /// Key used by builder
    const BUILDER_PRIVATE_KEY: &str =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    /// Key used in tests, so builder txs won't affect them
    const CUSTOM_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_chain_produces_blocks() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_chain_produces_blocks").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1234)
            .network_port(1235)
            .http_port(1238)
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1234").unwrap();
        let validation_api = EngineApi::new("http://localhost:1236").unwrap();

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1, None);
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .connect_http("http://localhost:1238".parse()?);

        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            for hash in block.transactions.hashes() {
                let _ = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
            }
        }

        // there must be a line logging the monitoring transaction
        op_rbuilder
            .find_log_line("Committed block built by builder")
            .await?;

        Ok(())
    }

    /// Caution: this test could be brittle to change because of queued pool checks
    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_revert_protection() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_revert_protection").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1244)
            .network_port(1245)
            .http_port(1248)
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1246)
            .network_port(1247);

        framework.start("op-reth", &reth).await.unwrap();

        let _ = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1244").unwrap();
        let validation_api = EngineApi::new("http://localhost:1246").unwrap();

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1, None);
        let latest_block = generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .connect_http("http://localhost:1248".parse()?);

        let mut base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );
        for _ in 0..10 {
            // We check queued pool to see if it already has tx we sent earlier
            // We do this in the beginning, because once we insert valid tx the queued tx would move
            // to pending pool, because nonce gap would be fixed.
            let mut queued_hash = None;
            let mut pool_content = provider.txpool_content().await?;
            if !pool_content.queued.is_empty() {
                // We are on the non-first, so we have 1 tx in queue pool
                assert_eq!(
                    pool_content.queued.len(),
                    1,
                    "Queued pool should contain only 1 transaction"
                );
                queued_hash = Some(
                    pool_content
                        .queued
                        .pop_first()
                        .unwrap()
                        .1
                        .pop_first()
                        .unwrap()
                        .1
                        .tx_hash(),
                );
            }

            let known_wallet = Signer::try_from_secret(CUSTOM_PRIVATE_KEY.parse()?)?;
            // Get builder's address
            let custom_address = known_wallet.address;
            // Get current nonce from chain
            let nonce = provider.get_transaction_count(custom_address).await?;

            // Send valid tx that must be included in the block
            let valid_tx = {
                let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce,
                    gas_limit: 210000,
                    max_fee_per_gas: base_fee.into(),
                    ..Default::default()
                });
                let signed_tx = known_wallet.sign_tx(tx_request)?;
                let known_tx = provider
                    .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                    .await?;
                known_tx.tx_hash().to_owned()
            };

            // If we don't have reverting tx in the queued pool we send one. This send occurs only
            // on first cycle iteration
            let revert_tx_1 = if queued_hash.is_none() {
                // Reverting tx that would be removed
                let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce: nonce + 1,
                    gas_limit: 300000,
                    max_fee_per_gas: base_fee.into(),
                    input: hex!("60006000fd").into(), // PUSH1 0x00 PUSH1 0x00 REVERT
                    ..Default::default()
                });
                let signed_tx = known_wallet.sign_tx(tx_request)?;
                let reverting_tx = provider
                    .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                    .await?;
                reverting_tx.tx_hash().to_owned()
            } else {
                queued_hash.unwrap()
            };

            // We send second reverting tx
            let revert_tx_2 = {
                // Reverting tx that would be places in queue pool, then it would be placed in the
                // pending pool in the next interation (after the nonce gap is fixed be sending
                // valid tx) and removed
                let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce: nonce + 2,
                    gas_limit: 300000,
                    max_fee_per_gas: base_fee.into(),
                    input: hex!("60006000fd").into(), // PUSH1 0x00 PUSH1 0x00 REVERT
                    ..Default::default()
                });
                let signed_tx = known_wallet.sign_tx(tx_request)?;
                let reverting_tx_2 = provider
                    .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                    .await?;
                reverting_tx_2.tx_hash().to_owned()
            };

            // All txs should be in pending pool.
            // 2 cases:
            // - we sent all 3 txs
            // - we sent 2 txs and one got promoted from queue pool
            let pool = provider.txpool_status().await?;
            assert_eq!(pool.pending, 3, "all txs should be in pending pool");
            assert_eq!(pool.queued, 0, "queued pool should be empty");

            let block_hash = generator.generate_block().await?;

            // After block is produced  we will remove one of the reverting txs and place another
            // in queue pool because we have nonce gap
            let pool = provider.txpool_status().await?;
            assert_eq!(pool.pending, 0, "pending pool should be empty");
            assert_eq!(pool.queued, 1, "queued pool should contain 1 tx");

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            // Verify valid transaction is included
            assert!(
                block.transactions.hashes().any(|hash| hash == *valid_tx),
                "successful transaction missing from block"
            );

            // Verify reverting transactions are NOT included
            assert!(
                !block
                    .transactions
                    .hashes()
                    .any(|hash| hash == *revert_tx_1 || hash == *revert_tx_2),
                "reverted transaction unexpectedly included in block"
            );
            for hash in block.transactions.hashes() {
                let receipt = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
                let success = receipt.inner.inner.status();
                assert!(success);
            }
            base_fee = max(
                block.header.base_fee_per_gas.unwrap(),
                MIN_PROTOCOL_BASE_FEE,
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_fee_priority_ordering() -> eyre::Result<()> {
        // This test validates that transactions are ordered by fee priority in blocks
        let mut framework =
            IntegrationFramework::new("integration_test_fee_priority_ordering").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1264)
            .network_port(1265)
            .http_port(1268)
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1266)
            .network_port(1267);

        framework.start("op-reth", &reth).await.unwrap();

        let _ = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1264").unwrap();
        let validation_api = EngineApi::new("http://localhost:1266").unwrap();

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1, None);
        let latest_block = generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .connect_http("http://localhost:1268".parse()?);

        let base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );

        // Create transactions with increasing fee values
        let priority_fees: [u128; 5] = [1, 3, 5, 2, 4]; // Deliberately not in order
        let signers = vec![
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
        ];
        let mut txs = Vec::new();

        // Fund test accounts with deposits
        for signer in &signers {
            generator
                .deposit(signer.address, 1000000000000000000)
                .await?;
        }

        // Send transactions in non-optimal fee order
        for (i, priority_fee) in priority_fees.iter().enumerate() {
            let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                chain_id: 901,
                nonce: 1,
                gas_limit: 210000,
                max_fee_per_gas: base_fee as u128 + *priority_fee,
                max_priority_fee_per_gas: *priority_fee,
                ..Default::default()
            });
            let signed_tx = signers[i].sign_tx(tx_request)?;
            let tx = provider
                .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                .await?;
            txs.push(tx);
        }

        // Generate a block that should include these transactions
        let block_hash = generator.generate_block().await?;

        // Query the block and check transaction ordering
        let block = provider
            .get_block_by_hash(block_hash)
            .full()
            .await?
            .expect("block");

        // Verify all transactions are included
        for tx in &txs {
            assert!(
                block
                    .transactions
                    .hashes()
                    .any(|hash| hash == *tx.tx_hash()),
                "transaction missing from block"
            );
        }

        let tx_fees: Vec<_> = block
            .transactions
            .into_transactions()
            .map(|tx| tx.effective_tip_per_gas(base_fee.into()))
            .collect();

        // Verify transactions are ordered by decreasing fee (highest fee first)
        // Skip the first deposit transaction and last builder transaction
        for i in 1..tx_fees.len() - 2 {
            assert!(
                tx_fees[i] >= tx_fees[i + 1],
                "Transactions not ordered by decreasing fee: {:?}",
                tx_fees
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "flashblocks")]
    async fn integration_test_chain_produces_blocks() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_chain_produces_blocks").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1234)
            .network_port(1235)
            .http_port(1238)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_flashblocks_ws_url("localhost:1239")
            .with_chain_block_time(2000)
            .with_flashbots_block_time(200);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        // Create a struct to hold received messages
        let received_messages = Arc::new(Mutex::new(Vec::new()));
        let messages_clone = received_messages.clone();

        // Spawn WebSocket listener task
        let ws_handle = tokio::spawn(async move {
            let (ws_stream, _) = connect_async("ws://localhost:1239").await?;
            let (_, mut read) = ws_stream.split();

            while let Some(Ok(msg)) = read.next().await {
                if let Ok(text) = msg.into_text() {
                    messages_clone.lock().unwrap().push(text);
                }
            }
            Ok::<_, eyre::Error>(())
        });

        let engine_api = EngineApi::new("http://localhost:1234").unwrap();
        let validation_api = EngineApi::new("http://localhost:1236").unwrap();

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 2, None);
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .connect_http("http://localhost:1238".parse()?);

        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            for hash in block.transactions.hashes() {
                let _ = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
            }
        }
        // there must be a line logging the monitoring transaction
        op_rbuilder
            .find_log_line("Processing new chain commit") // no builder tx for flashblocks builder
            .await?;

        // check there's 10 flashblocks log lines (2000ms / 200ms)
        op_rbuilder.find_log_line("Building flashblock 9").await?;

        // Process websocket messages
        let timeout_duration = Duration::from_secs(10);
        tokio::time::timeout(timeout_duration, async {
            let mut message_count = 0;
            loop {
                if message_count >= 10 {
                    break;
                }
                let messages = received_messages.lock().unwrap();
                let messages_json: Vec<serde_json::Value> = messages
                    .iter()
                    .map(|msg| serde_json::from_str(msg).unwrap())
                    .collect();
                for msg in messages_json.iter() {
                    let metadata = msg.get("metadata");
                    assert!(metadata.is_some(), "metadata field missing");
                    let metadata = metadata.unwrap();
                    assert!(
                        metadata.get("block_number").is_some(),
                        "block_number missing"
                    );
                    assert!(
                        metadata.get("new_account_balances").is_some(),
                        "new_account_balances missing"
                    );
                    assert!(metadata.get("receipts").is_some(), "receipts missing");
                    // also check if the length of the receipts is the same as the number of transactions
                    assert!(
                        metadata.get("receipts").unwrap().as_object().unwrap().len()
                            == msg
                                .get("diff")
                                .unwrap()
                                .get("transactions")
                                .unwrap()
                                .as_array()
                                .unwrap()
                                .len(),
                        "receipts length mismatch"
                    );
                    message_count += 1;
                }
                drop(messages);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
        ws_handle.abort();

        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "flashblocks")]
    async fn integration_test_flashblocks_respects_gas_limit() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_flashblocks_respects_gas_limit").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        let block_time_ms = 1000;
        let flashblock_time_ms = 100;

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1244)
            .network_port(1245)
            .http_port(1248)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_flashblocks_ws_url("localhost:1249")
            .with_chain_block_time(block_time_ms)
            .with_flashbots_block_time(flashblock_time_ms);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1246)
            .network_port(1247);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1244").unwrap();
        let validation_api = EngineApi::new("http://localhost:1246").unwrap();

        let mut generator = BlockGenerator::new(
            &engine_api,
            Some(&validation_api),
            false,
            block_time_ms / 1000,
            None,
        );
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1248".parse()?);

        // Delay the payload building by 4s, ensure that the correct number of flashblocks are built
        let block_hash = generator.generate_block_with_delay(4).await?;

        // query the block and the transactions inside the block
        let block = provider
            .get_block_by_hash(block_hash)
            .await?
            .expect("block");

        for hash in block.transactions.hashes() {
            let _ = provider
                .get_transaction_receipt(hash)
                .await?
                .expect("receipt");
        }

        op_rbuilder
            .find_log_line("Processing new chain commit") // no builder tx for flashblocks builder
            .await?;

        // check there's no more than 10 flashblocks log lines (2000ms / 200ms)
        op_rbuilder.find_log_line("Building flashblock 9").await?;
        op_rbuilder
            .find_log_line("Skipping flashblock reached target=10 idx=10")
            .await?;

        Ok(())
    }
}
