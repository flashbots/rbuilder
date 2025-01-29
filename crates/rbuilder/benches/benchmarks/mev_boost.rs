use alloy_eips::eip4844::BlobTransactionSidecar;
use alloy_primitives::{BlockHash, U256};
use criterion::{criterion_group, Criterion};
use primitive_types::H384;
use rbuilder::mev_boost::{
    rpc::TestDataGenerator, sign_block_for_relay, submission::DenebSubmitBlockRequest,
    BLSBlockSigner,
};
use reth::primitives::SealedBlock;
use reth_chainspec::SEPOLIA;
use reth_primitives::{kzg::Blob, SealedHeader};
use std::{fs, path::PathBuf, sync::Arc};

fn mev_boost_serialize_submit_block(data: DenebSubmitBlockRequest) {
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
        blobs.push(Arc::new(blob.clone()));
    }

    let chain_spec = SEPOLIA.clone();
    let payload = generator.create_payload_attribute_data();

    let mut group = c.benchmark_group("MEV-Boost Sign block for relay");

    // This benchmark is here to have a baseline for Deneb (with blobs)
    group.bench_function("Capella", |b| {
        b.iter(|| {
            let _ = sign_block_for_relay(
                &signer,
                &sealed_block,
                &blobs,
                &Vec::new(),
                &chain_spec,
                &payload,
                H384::default(),
                U256::default(),
            )
            .unwrap();
        })
    });

    // Create a sealed block that is after the Cancun hard fork in Sepolia
    // this is, a timestamp higher than 1706655072
    let mut sealed_block_deneb: SealedBlock<alloy_consensus::Header, reth_primitives::BlockBody> =
        SealedBlock::default();
    let mut header = sealed_block_deneb.header().clone();
    header.timestamp = 2706655072;
    header.blob_gas_used = Some(64);
    header.excess_blob_gas = Some(64);
    sealed_block_deneb.header = SealedHeader::new(header.clone(), BlockHash::default());

    group.bench_function("Deneb", |b| {
        b.iter(|| {
            let _ = sign_block_for_relay(
                &signer,
                &sealed_block_deneb,
                &blobs,
                &Vec::new(),
                &chain_spec,
                &payload,
                H384::default(),
                U256::default(),
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
