use std::path::PathBuf;

use eth_sparse_mpt::{
    test_utils::deserialize_from_json_gzip,
    v1::reth_sparse_trie::{change_set::ETHTrieChangeSet, hash::EthSparseTries},
    RootHashThreadPool,
};
use std::time::Instant;

fn main() {
    let mut examples = Vec::new();

    for i in 0..7 {
        let dir: PathBuf = format!("./test_data/prepared_tries/example{}/", i)
            .parse()
            .unwrap();

        let change_set: ETHTrieChangeSet = {
            let mut p = dir.clone();
            p.push("change_set.json.gz");
            deserialize_from_json_gzip(p).expect("change set")
        };

        let tries: EthSparseTries = {
            let mut p = dir.clone();
            p.push("tries.json.gz");
            deserialize_from_json_gzip(p).expect("sparse trie")
        };

        examples.push((change_set, tries));
    }

    const WARMUP_RUNS: usize = 100;
    const REAL_RUNS: usize = 1000;

    const PAR_ACCOUNT_TRIE: bool = true;
    const PAR_STORAGE_TRIES: bool = true;

    let threadpool = RootHashThreadPool::try_new(4).unwrap();

    println!("example,min,max,p50,p99,MAX/MIN,p99/p50");
    for (i, (change_set, tries)) in examples.into_iter().enumerate() {
        let mut measures = Vec::new();

        for _ in 0..WARMUP_RUNS {
            let (change_set, mut tries) = (change_set.clone(), tries.clone());

            threadpool.rayon_pool.install(|| {
                tries
                    .calculate_root_hash(change_set, PAR_STORAGE_TRIES, PAR_ACCOUNT_TRIE)
                    .unwrap();
            });
        }

        for _ in 0..REAL_RUNS {
            let (change_set, mut tries) = (change_set.clone(), tries.clone());

            let start = Instant::now();

            threadpool.rayon_pool.install(|| {
                tries
                    .calculate_root_hash(change_set, PAR_STORAGE_TRIES, PAR_ACCOUNT_TRIE)
                    .unwrap();
            });
            measures.push(start.elapsed().as_micros());
        }

        measures.sort();
        let min = *measures.first().unwrap();
        let max = *measures.last().unwrap();
        let p50 = measures[measures.len() / 2];
        let p99 = measures[measures.len() * 99 / 100];

        println!(
            "{},{},{},{},{},{:.2},{:.2}",
            i,
            min,
            max,
            p50,
            p99,
            max as f64 / min as f64,
            p99 as f64 / p50 as f64
        );
    }
}
