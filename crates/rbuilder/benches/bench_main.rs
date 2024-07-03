use criterion::criterion_main;

mod benchmarks;

criterion_main! {
    benchmarks::mev_boost::serialization,
}
