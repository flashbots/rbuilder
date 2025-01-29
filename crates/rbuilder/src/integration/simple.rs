#[cfg(test)]
mod tests {
    use crate::integration::playground::Playground;

    use alloy_network::TransactionBuilder;
    use alloy_primitives::U256;
    use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use std::str::FromStr;
    use test_utils::ignore_if_env_not_set;
    use url::Url;

    async fn send_transaction(
        srv: &Playground,
        private_key: alloy_network::EthereumWallet,
        to: Option<alloy_primitives::Address>,
    ) -> eyre::Result<alloy_primitives::TxHash> {
        let rbuilder_provider =
            ProviderBuilder::new().on_http(Url::parse(srv.rbuilder_rpc_url()).unwrap());

        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(private_key)
            .on_http(Url::parse(srv.el_url()).unwrap());

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

    // #[ignore_if_env_not_set("PLAYGROUND")] // TODO: Change with a custom macro (i.e ignore_if_not_playground)
    #[tokio::test]
    async fn test_simple_example() {
        let srv = Playground::new().unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // Send transaction using the helper function
        let tx_hash = send_transaction(&srv, srv.prefunded_key(), None)
            .await
            .unwrap();

        // Wait for receipt
        let binding = ProviderBuilder::new().on_http(Url::parse(srv.el_url()).unwrap());
        let pending_tx = PendingTransactionBuilder::new(binding.clone(), tx_hash)
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
            let pending_tx = PendingTransactionBuilder::new(binding.clone(), tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );

            assert!(
                srv.check_logs_contain(
                    "Transaction rejected - sender 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC is blocklisted"
                )
                .unwrap(),
                "Expected log not found"
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
            let pending_tx = PendingTransactionBuilder::new(binding, tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );

            assert!(
            srv.check_logs_contain(
                "Transaction rejected - recipient 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC is blocklisted"
            )
            .unwrap(),
            "Expected log not found"
        );
        }
    }
}
