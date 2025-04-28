use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_node_bindings::Anvil;
use alloy_primitives::U256;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use criterion::{criterion_group, Criterion};
use rbuilder::live_builder::order_input::{
    txpool_fetcher::subscribe_to_txpool_with_blobs, OrderInputConfig,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

async fn txpool_receive_util(count: u32) {
    let anvil = Anvil::new()
        .args(["--ipc", "/tmp/anvil.ipc"])
        .try_spawn()
        .unwrap();

    let (sender, mut receiver) = mpsc::channel(10);
    subscribe_to_txpool_with_blobs(
        OrderInputConfig::default_e2e(),
        sender,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(anvil.endpoint().parse().unwrap());

    let alice = anvil.addresses()[0];
    let eip1559_est = provider.estimate_eip1559_fees().await.unwrap();

    let tx = TransactionRequest::default()
        .with_to(alice)
        .with_value(U256::from(1))
        .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
        .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas);

    tokio::spawn(async move {
        for i in 0..count {
            let tx = tx.clone().with_nonce(i.into());
            let _ = provider.send_transaction(tx).await.unwrap();
        }
    });

    for _ in 0..count {
        let _ = receiver.recv().await.unwrap();
    }
}

fn bench_txpool_receive(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("Txpool fetcher");

    group.measurement_time(Duration::from_secs(20));
    group.bench_function("txn_fetcher_normal_10", |b| {
        b.to_async(&rt).iter(|| txpool_receive_util(10));
    });
    group.bench_function("txn_fetcher_normal_50", |b| {
        b.to_async(&rt).iter(|| txpool_receive_util(50));
    });
}

criterion_group!(txpool, bench_txpool_receive,);
