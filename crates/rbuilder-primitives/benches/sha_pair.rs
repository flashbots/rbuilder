use alloy_primitives::B256;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use proptest::{prelude::*, strategy::ValueTree as _, test_runner::TestRunner};

criterion_main!(sha_pair);
criterion_group!(sha_pair, sha_pair_bench);

fn sha_pair_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("sha_pair");

    // Start with asserting equivalence of all implementations.
    impls::assert_equivalence();

    for size in [100, 1_000, 100_000] {
        let mut runner = TestRunner::deterministic();
        let pairs = proptest::collection::vec(any::<(B256, B256)>(), size)
            .new_tree(&mut runner)
            .unwrap()
            .current();

        group.bench_function(BenchmarkId::new("sha2", size), |b| {
            b.iter(|| {
                pairs.iter().for_each(|(a, b)| {
                    impls::sha2_sha_pair(a, b);
                });
            });
        });

        group.bench_function(BenchmarkId::new("sha2 buf", size), |b| {
            b.iter_with_setup(
                || [0u8; 64],
                |mut buf| {
                    pairs.iter().for_each(|(a, b)| {
                        impls::sha2_sha_pair_buf(&mut buf, a, b);
                    });
                },
            );
        });

        group.bench_function(BenchmarkId::new("ring", size), |b| {
            b.iter(|| {
                pairs.iter().for_each(|(a, b)| {
                    impls::ring_sha_pair(a, b);
                });
            });
        });

        group.bench_function(BenchmarkId::new("ring buf", size), |b| {
            b.iter_with_setup(
                || [0u8; 64],
                |mut buf| {
                    pairs.iter().for_each(|(a, b)| {
                        impls::ring_sha_pair_buf(&mut buf, a, b);
                    });
                },
            );
        });
    }
}

mod impls {
    use super::*;

    pub fn assert_equivalence() {
        let mut buf = [0u8; 64];
        for _ in 0..100 {
            let a = B256::random();
            let b = B256::random();

            let expected = sha2_sha_pair(&a, &b);
            assert_eq!(expected, sha2_sha_pair_buf(&mut buf, &a, &b));
            assert_eq!(expected, ring_sha_pair(&a, &b));
            assert_eq!(expected, ring_sha_pair_buf(&mut buf, &a, &b));
        }
    }

    #[inline]
    pub fn sha2_sha_pair(a: &B256, b: &B256) -> B256 {
        use sha2::{Digest, Sha256};

        let mut h = Sha256::new();
        h.update(a);
        h.update(b);
        B256::from_slice(&h.finalize())
    }

    #[inline]
    pub fn sha2_sha_pair_buf(buf: &mut [u8; 64], a: &B256, b: &B256) -> B256 {
        use sha2::{Digest, Sha256};

        // one update with a single 64-byte buffer tends to be a tiny bit faster
        buf[..32].copy_from_slice(a.as_slice());
        buf[32..].copy_from_slice(b.as_slice());

        let mut h = Sha256::new();
        h.update(buf);
        B256::from_slice(&h.finalize())
    }

    #[inline]
    pub fn ring_sha_pair(a: &B256, b: &B256) -> B256 {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(a.as_slice());
        buf[32..].copy_from_slice(b.as_slice());
        let out = ring::digest::digest(&ring::digest::SHA256, &buf);
        B256::from_slice(out.as_ref())
    }

    #[inline]
    pub fn ring_sha_pair_buf(buf: &mut [u8; 64], a: &B256, b: &B256) -> B256 {
        buf[..32].copy_from_slice(a.as_slice());
        buf[32..].copy_from_slice(b.as_slice());
        let out = ring::digest::digest(&ring::digest::SHA256, buf.as_slice());
        B256::from_slice(out.as_ref())
    }
}
