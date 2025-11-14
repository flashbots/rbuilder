#[cfg(test)]
mod tests {
    use crate::{
        integration::playground::Playground,
        live_builder::{
            block_list_provider::test::{BlocklistHttpServer, BLOCKLIST_LEN_2},
            process_killer::MAX_WAIT_TIME_SECONDS,
        },
    };

    use alloy_network::TransactionBuilder;
    use alloy_primitives::U256;
    use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use std::{path::PathBuf, str::FromStr, time::Duration};
    use test_utils::ignore_if_env_not_set;
    use url::Url;

    async fn send_transaction(
        srv: &Playground,
        private_key: alloy_network::EthereumWallet,
        to: Option<alloy_primitives::Address>,
    ) -> eyre::Result<alloy_primitives::TxHash> {
        let rbuilder_provider =
            ProviderBuilder::new().connect_http(Url::parse(srv.rbuilder_rpc_url()).unwrap());

        let provider = ProviderBuilder::new()
            .wallet(private_key)
            .connect_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await?;

        let tx = TransactionRequest::default()
            .with_to(to.unwrap_or(srv.builder_address()))
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);

        let tx = provider.fill(tx).await?;

        // send the transaction ONLY to the builder
        let pending_tx = rbuilder_provider
            .send_tx_envelope(tx.as_envelope().unwrap().clone())
            .await?;

        Ok(*pending_tx.tx_hash())
    }

    #[ignore_if_env_not_set("PLAYGROUND")] // TODO: Change with a custom macro (i.e ignore_if_not_playground)
    #[tokio::test]
    async fn test_simple_example() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rbuilder/src/integration/test_data/config-playground.toml");

        // This test sends a transaction ONLY to the builder and waits for the block to be built with it.
        let srv = Playground::new("test_simple_example", &config_path).unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // Send transaction using the helper function
        let tx_hash = send_transaction(&srv, srv.prefunded_key(), None)
            .await
            .unwrap();

        // Wait for receipt
        let binding = ProviderBuilder::new().connect_http(Url::parse(srv.el_url()).unwrap());
        let pending_tx = PendingTransactionBuilder::new(binding.root().clone(), tx_hash)
            .with_timeout(Some(std::time::Duration::from_secs(60)));

        let receipt = pending_tx.get_receipt().await.unwrap();
        srv.validate_block_built(receipt.block_number.unwrap())
            .await
            .unwrap();

        // Send a transaction with an account from the blocklist
        // TODO: This should be a separated test but the integration framework does use fixed port numbers
        // and we need to change it to use dynamic ports.
        // Since we only send the transaction to the builder, it should never be included in the block.
        {
            srv.wait_for_next_slot().await.unwrap();
            let tx_hash = send_transaction(&srv, srv.blocklist_key(), None)
                .await
                .unwrap();

            // wait for 20 seconds
            let pending_tx = PendingTransactionBuilder::new(binding.root().clone(), tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );
        }

        // Second blocklist test, send a transaction from a non-blocklisted account to a blocklisted account
        {
            srv.wait_for_next_slot().await.unwrap();
            let tx_hash =
                send_transaction(&srv, srv.prefunded_key(), Some(srv.blocklist_address()))
                    .await
                    .unwrap();

            // wait for 20 seconds
            let pending_tx = PendingTransactionBuilder::new(binding.root().clone(), tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );
        }
    }

    #[ignore_if_env_not_set("PLAYGROUND")]
    /// TODO: Change with a custom macro (i.e ignore_if_not_playground)
    /// Sadly builder shutdown does not always work properly so we have to wait for the watchdog to kill the process.
    #[tokio::test]
    async fn test_builder_closes_on_old_blocklist() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../crates/rbuilder/src/integration/test_data/config-playground-http-blocklist.toml",
        );
        let blocklist_server = BlocklistHttpServer::new(1934, Some(BLOCKLIST_LEN_2.to_string()));
        tokio::time::sleep(Duration::from_millis(100)).await; //puaj
        let mut srv =
            Playground::new("test_builder_closes_on_old_blocklist", &config_path).unwrap();
        srv.wait_for_next_slot().await.unwrap();
        blocklist_server.set_answer(None);
        let timeout_secs = 5 /*blocklist_url_max_age_secs in cfg */ +
             12 /* problem detected in next block start an cancel is signaled*/+
             15 /*watchdog_timeout_sec */+
             MAX_WAIT_TIME_SECONDS /*extra delay from letting the builder finish its work*/+
             1 /* for timing errors */;
        tokio::time::sleep(Duration::from_secs(timeout_secs)).await; //puaj
        assert!(!srv.builder_is_alive());
    }
}
