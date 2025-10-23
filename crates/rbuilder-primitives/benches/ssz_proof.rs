use alloy_primitives::Bytes;
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, Criterion,
};
use proptest::{prelude::*, strategy::ValueTree as _, test_runner::TestRunner};

use impls::SszTransactionProof;

criterion_main!(ssz_proof);
criterion_group!(ssz_proof, ssz_proof_bench);

fn ssz_proof_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("ssz_proof");

    // Start with asserting equivalence of all implementations.
    impls::assert_equivalence();

    for num_txs in [100, 500, 1_000] {
        let target = num_txs - 1;

        for tx_size in [128, 1_024] {
            let mut runner = TestRunner::deterministic();
            let txs = generate_test_data(&mut runner, num_txs, tx_size);

            run_bench::<impls::VanillaSszTxProof>(&mut group, &txs, target);
            run_bench::<impls::VanillaBufferedSszTxProof>(&mut group, &txs, target);
            run_bench::<impls::CompactSszTxProof>(&mut group, &txs, target);
        }
    }
}

fn run_bench<T: SszTransactionProof>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    txs: &[Bytes],
    target: usize,
) {
    let tx_size = txs.first().unwrap().0.len();
    let id = format!(
        "{} | num txs {} | tx size {} bytes",
        T::description(),
        txs.len(),
        tx_size
    );
    group.bench_function(id, |b| {
        b.iter_with_setup(
            || T::default(),
            |mut gen| {
                gen.generate(txs, target);
            },
        )
    });
}

fn generate_test_data(runner: &mut TestRunner, num_txs: usize, tx_size: usize) -> Vec<Bytes> {
    proptest::collection::vec(proptest::collection::vec(any::<u8>(), tx_size), num_txs)
        .new_tree(runner)
        .unwrap()
        .current()
        .into_iter()
        .map(Bytes::from)
        .collect::<Vec<_>>()
}

mod impls {
    use super::*;
    use alloy_primitives::{Bytes, B256};
    use rbuilder_primitives::mev_boost::ssz_roots::{
        sha_pair, tx_ssz_leaf_root, CompactSszTransactionTree,
    };

    const TREE_DEPTH: usize = 20; // log₂(MAX_TRANSACTIONS_PER_PAYLOAD)
    const MAX_CHUNK_COUNT: usize = 1 << TREE_DEPTH;

    pub fn assert_equivalence() {
        let num_txs = 100;
        let proof_target = num_txs - 1;
        let tx_size = 1_024;
        let mut runner = TestRunner::deterministic();

        let mut vanilla = VanillaSszTxProof::default();
        let mut vanilla_buf = VanillaBufferedSszTxProof::default();
        let mut compact = CompactSszTxProof::default();
        for _ in 0..100 {
            let txs = generate_test_data(&mut runner, num_txs, tx_size);
            let expected = vanilla.generate(&txs, proof_target);
            assert_eq!(expected, vanilla_buf.generate(&txs, proof_target));
            assert_eq!(expected, compact.generate(&txs, proof_target));
        }
    }

    pub trait SszTransactionProof: Default {
        fn description() -> &'static str;

        fn generate(&mut self, txs: &[Bytes], target: usize) -> Vec<B256>;
    }

    /// === VanillaSszTransactionProof ===
    #[derive(Default)]
    pub struct VanillaSszTxProof;

    impl SszTransactionProof for VanillaSszTxProof {
        fn description() -> &'static str {
            "vanilla"
        }

        fn generate(&mut self, txs: &[Bytes], target: usize) -> Vec<B256> {
            vanilla_transaction_proof_ssz(txs, target, &mut Vec::new(), &mut Vec::new())
        }
    }

    /// === VanillaBufferedSszTransactionProof ===
    #[derive(Default)]
    pub struct VanillaBufferedSszTxProof {
        current_buf: Vec<B256>,
        next_buf: Vec<B256>,
    }

    impl SszTransactionProof for VanillaBufferedSszTxProof {
        fn description() -> &'static str {
            "vanilla with buffers"
        }

        fn generate(&mut self, txs: &[Bytes], target: usize) -> Vec<B256> {
            vanilla_transaction_proof_ssz(txs, target, &mut self.current_buf, &mut self.next_buf)
        }
    }

    fn vanilla_transaction_proof_ssz(
        txs: &[Bytes],
        target: usize,
        current_buf: &mut Vec<B256>,
        next_buf: &mut Vec<B256>,
    ) -> Vec<B256> {
        current_buf.clear();
        for idx in 0..MAX_CHUNK_COUNT {
            let leaf = txs
                .get(idx)
                .map(|tx| tx_ssz_leaf_root(&tx))
                .unwrap_or(B256::ZERO);
            current_buf.insert(idx, leaf);
        }

        let mut branch = Vec::new();
        let (current_level, next_level) = (current_buf, next_buf);
        let mut current_index = target;

        for _level in 0..TREE_DEPTH {
            let sibling_index = current_index ^ 1;
            branch.push(current_level[sibling_index]);

            next_level.clear();
            for i in (0..current_level.len()).step_by(2) {
                let left = current_level[i];
                let right = current_level[i + 1];
                next_level.push(sha_pair(&left, &right));
            }

            std::mem::swap(current_level, next_level);
            current_index /= 2;

            if current_level.len() == 1 {
                break;
            }
        }

        branch
    }

    /// === CompactSszTxProof ===
    #[derive(Default)]
    pub struct CompactSszTxProof;

    impl SszTransactionProof for CompactSszTxProof {
        fn description() -> &'static str {
            "compact"
        }

        fn generate(&mut self, txs: &[Bytes], target: usize) -> Vec<B256> {
            let mut leaves = Vec::with_capacity(txs.len());
            for tx in txs {
                leaves.push(tx_ssz_leaf_root(tx));
            }
            CompactSszTransactionTree::from_leaves(leaves).proof(target)
        }
    }
}
