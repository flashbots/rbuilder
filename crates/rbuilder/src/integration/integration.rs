#[cfg(test)]
mod tests {
    use crate::integration::playground::FakeMevBoostRelay;

    use alloy_network::TransactionBuilder;
    use alloy_primitives::U256;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use std::str::FromStr;
    use test_utils::ignore_if_env_not_set;
    use url::Url;

    #[ignore_if_env_not_set("PLAYGROUND_DIR")]
    #[tokio::test]
    async fn test_simple_example() {
        let srv = FakeMevBoostRelay::new().unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // send a transfer to the builder
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(srv.prefunded_keys(0))
            .on_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await.unwrap();

        let tx = TransactionRequest::default()
            .with_to(srv.builder_address())
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);

        let pending_tx = provider.send_transaction(tx).await.unwrap();
        let receipt = pending_tx.get_receipt().await.unwrap();

        srv.validate_block_built(receipt.block_number.unwrap())
            .await
            .unwrap();
    }
}
