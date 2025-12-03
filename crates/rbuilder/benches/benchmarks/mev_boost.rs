use alloy_consensus::{Block, Header};
use alloy_eips::{eip4844::BlobTransactionSidecar, eip7594::BlobTransactionSidecarVariant};
use alloy_primitives::U256;
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_rpc_types_beacon::BlsPublicKey;
use criterion::{criterion_group, Criterion};
use rbuilder::mev_boost::{rpc::TestDataGenerator, sign_block_for_relay, BLSBlockSigner};
use reth::primitives::SealedBlock;
use reth_primitives::kzg::Blob;
use ssz::Encode;
use std::{fs, path::PathBuf, sync::Arc};

fn mev_boost_serialize_submit_block(data: AlloySubmitBlockRequest) {
    data.as_ssz_bytes();
}

fn bench_mevboost_serialization(c: &mut Criterion) {
    let mut generator = TestDataGenerator::default();
    let mut group = c.benchmark_group("MEV-Boost SubmitBlock serialization");

    group.bench_function("SSZ encoding", |b| {
        b.iter_batched(
            || generator.create_deneb_submit_block_request(),
            |b| {
                mev_boost_serialize_submit_block(b);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("JSON encoding", |b| {
        b.iter_batched(
            || generator.create_deneb_submit_block_request(),
            |b| {
                serde_json::to_vec(&b).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_mevboost_sign(c: &mut Criterion) {
    let mut generator = TestDataGenerator::default();

    let json_content = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/blob_data/blob1.json"),
    )
    .unwrap();

    // Parse the JSON contents into a serde_json::Value
    let json_value: serde_json::Value =
        serde_json::from_str(&json_content).expect("Failed to deserialize JSON");

    // Extract blob data from JSON and convert it to Blob
    let blobs: Vec<Blob> = vec![Blob::from_hex(
        json_value
            .get("data")
            .unwrap()
            .as_str()
            .expect("Data is not a valid string"),
    )
    .unwrap()];

    // Generate a BlobTransactionSidecar from the blobs
    let blob = BlobTransactionSidecar::try_from_blobs(blobs).unwrap();

    let sealed_block = SealedBlock::default();
    let signer = BLSBlockSigner::test_signer();
    let mut blobs = vec![];
    for _ in 0..3 {
        blobs.push(Arc::new(BlobTransactionSidecarVariant::Eip4844(
            blob.clone(),
        )));
    }

    let payload = generator.create_payload_attribute_data();

    let mut group = c.benchmark_group("MEV-Boost Sign block for relay");

    // This benchmark is here to have a baseline for Deneb (with blobs)
    group.bench_function("Capella", |b| {
        b.iter(|| {
            let _ = sign_block_for_relay(
                &signer,
                &sealed_block,
                &payload,
                BlsPublicKey::ZERO,
                U256::ZERO,
            )
            .unwrap();
        })
    });

    // Create a sealed block that is after the Cancun hard fork in Sepolia
    // this is, a timestamp higher than 1706655072
    let sealed_block_deneb = SealedBlock::new_unhashed(Block::new(
        Header {
            timestamp: 2706655072,
            blob_gas_used: Some(64),
            excess_blob_gas: Some(64),
            ..Default::default()
        },
        Default::default(),
    ));

    group.bench_function("Deneb", |b| {
        b.iter(|| {
            let _ = sign_block_for_relay(
                &signer,
                &sealed_block_deneb,
                &payload,
                BlsPublicKey::ZERO,
                U256::ZERO,
            )
            .unwrap();
        })
    });

    group.finish();
}

criterion_group!(
    serialization,
    bench_mevboost_serialization,
    bench_mevboost_sign
);
