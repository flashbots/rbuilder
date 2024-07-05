use criterion::{criterion_group, Criterion};
use pprof::criterion::{Output, PProfProfiler};

use ethereum_consensus::ssz::prelude::serialize;
use rbuilder::mev_boost::{
    marshal_deneb_submit_block_request, DenebSubmitBlockRequest, TestDataGenerator,
};

fn mev_boost_serialize_submit_block(data: DenebSubmitBlockRequest) {
    let marshalled_data = marshal_deneb_submit_block_request(&data).unwrap();
    serialize(&marshalled_data).unwrap();
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

fn bench_mevboost_serialization_flamegraphs(c: &mut Criterion) {
    let mut generator = TestDataGenerator::default();
    c.bench_function("JSON", |b| {
        b.iter_batched(
            || generator.create_deneb_submit_block_request(),
            |b| {
                serde_json::to_vec(&b).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = serialization;
    config = Criterion::default();
    targets = bench_mevboost_serialization
}
criterion_group!(
    name=serialization_flamegraph;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = bench_mevboost_serialization_flamegraphs
);
