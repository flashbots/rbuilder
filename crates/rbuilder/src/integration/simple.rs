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

    #[ignore_if_env_not_set("PLAYGROUND")] // TODO: Change with a custom macro (i.e ignore_if_not_playground)
    #[tokio::test]
    async fn test_simple_example() {
        // This test sends a transaction ONLY to the builder and waits for the block to be built with it.
        let srv = Playground::new().unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // send a transfer to the builder
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(srv.prefunded_key())
            .on_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await.unwrap();

        let tx = TransactionRequest::default()
            .with_to(srv.builder_address())
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);

        let tx = provider.fill(tx).await.unwrap();

        // send the transaction ONLY to the builder
        let rbuilder_provider =
            ProviderBuilder::new().on_http(Url::parse(srv.rbuilder_rpc_url()).unwrap());
        let pending_tx = rbuilder_provider
            .send_tx_envelope(tx.as_envelope().unwrap().clone())
            .await
            .unwrap();

        // wait for the transaction in the el node since rbuilder does not implement
        // the `eth_getTransactionReceipt` method.
        let binding = ProviderBuilder::new().on_http(Url::parse(srv.el_url()).unwrap());
        let pending_tx = PendingTransactionBuilder::new(&binding, *pending_tx.tx_hash())
            .with_timeout(Some(std::time::Duration::from_secs(60)));

        let receipt = pending_tx.get_receipt().await.unwrap();
        srv.validate_block_built(receipt.block_number.unwrap())
            .await
            .unwrap();
    }
}
