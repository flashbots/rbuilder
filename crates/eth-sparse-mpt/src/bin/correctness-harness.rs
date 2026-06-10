use alloy_primitives::B256;
use eth_sparse_mpt::{
    calculate_root_hash_with_sparse_trie, ETHSpareMPTVersion, SparseTrieLocalCache,
    SparseTrieSharedCache,
};
use eyre::{bail, Context, Result};
use reth_chainspec::{ChainSpec, DEV, HOLESKY, HOODI, MAINNET, SEPOLIA};
use reth_db::{
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
    open_db_read_only, ClientVersion, DatabaseEnv,
};
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use reth_provider::{
    providers::{
        ConsistentDbView, OverlayStateProviderFactory, RocksDBProvider, StaticFileProvider,
        StaticFileProviderBuilder,
    },
    BlockHashReader, BlockNumReader, BlockReader, ChainSpecProvider, HeaderProvider,
    ProviderFactory, TransactionVariant,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie::TrieInput;
use reth_trie_db::ChangesetCache;
use reth_trie_parallel::root::ParallelStateRoot;
use revm::database::BundleState;
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Instant,
};

type NodeTypes = NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>;
type Factory = ProviderFactory<NodeTypes>;

#[derive(Debug, Clone)]
struct Cli {
    full: bool,
    blocks: usize,
    reth_bin: PathBuf,
    datadir: PathBuf,
    chain: String,
    tip_fallback: bool,
    static_file_blocks_per_file: Option<u64>,
    skip_v2: bool,
    skip_v1: bool,
}

#[derive(Debug, Clone, Copy)]
enum CompareMode {
    TipPlusOne,
    TipFallback,
    CurrentTip,
}

#[derive(Debug)]
struct CompareResult {
    mode: CompareMode,
    parent_block: u64,
    target_block: u64,
    canonical_root: B256,
    reth_root: B256,
    v2_root: Option<B256>,
    v_experimental_root: B256,
    v1_root: Option<B256>,
    execute_ms: f64,
    reth_ms: f64,
    v2_ms: Option<f64>,
    v_experimental_ms: f64,
    v1_ms: Option<f64>,
    v2_fetched_nodes: Option<usize>,
    v_experimental_fetched_nodes: usize,
    v1_fetched_nodes: Option<usize>,
}

fn main() -> Result<()> {
    let cli = parse_cli()?;

    if cli.full {
        run_full_mode(&cli)?;
    } else {
        if cli.blocks != 1 {
            bail!("--blocks is only supported with --full");
        }
        let factory =
            open_provider_factory(&cli.datadir, &cli.chain, cli.static_file_blocks_per_file)?;
        let result = compare_tip_plus_one(&factory, &cli)?;
        print_compare_result(None, &result);
        println!("correctness check passed");
    }
    Ok(())
}

fn run_full_mode(cli: &Cli) -> Result<()> {
    println!(
        "running full mode: blocks={} (compare current tip, then unwind-by-1 each iteration)",
        cli.blocks
    );

    for i in 0..cli.blocks {
        let factory =
            open_provider_factory(&cli.datadir, &cli.chain, cli.static_file_blocks_per_file)?;
        let result = compare_current_tip(&factory, cli)?;
        print_compare_result(Some(i + 1), &result);
        if i + 1 < cli.blocks {
            run_unwind_once(cli, i + 1)?;
        }
    }

    println!("full correctness loop passed for {} iterations", cli.blocks);
    Ok(())
}

fn compare_tip_plus_one(factory: &Factory, cli: &Cli) -> Result<CompareResult> {
    let parent_block = factory
        .best_block_number()
        .context("failed to read best (executed) block number")?;
    let target_block = parent_block
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("tip overflow"))?;
    let tip_plus_one_exists = factory
        .header_by_number(target_block)
        .with_context(|| format!("failed to fetch target header for block {target_block}"))?
        .is_some();
    if tip_plus_one_exists {
        compare_block(
            factory,
            parent_block,
            target_block,
            CompareMode::TipPlusOne,
            cli.skip_v2,
            cli.skip_v1,
        )
    } else if cli.tip_fallback {
        let fallback_parent = parent_block
            .checked_sub(1)
            .ok_or_else(|| eyre::eyre!("cannot fallback to tip mode at block 0"))?;
        compare_block(
            factory,
            fallback_parent,
            parent_block,
            CompareMode::TipFallback,
            cli.skip_v2,
            cli.skip_v1,
        )
    } else {
        bail!(
            "target header block {} not found in local DB; this harness expects tip+1 to exist after unwind",
            target_block
        );
    }
}

fn compare_current_tip(factory: &Factory, cli: &Cli) -> Result<CompareResult> {
    let target_block = factory
        .best_block_number()
        .context("failed to read best (executed) block number")?;
    let parent_block = target_block
        .checked_sub(1)
        .ok_or_else(|| eyre::eyre!("tip is block 0; need at least one executed block in DB"))?;
    compare_block(
        factory,
        parent_block,
        target_block,
        CompareMode::CurrentTip,
        cli.skip_v2,
        cli.skip_v1,
    )
}

fn compare_block(
    factory: &Factory,
    parent_block: u64,
    target_block: u64,
    mode: CompareMode,
    skip_v2: bool,
    skip_v1: bool,
) -> Result<CompareResult> {
    if parent_block == 0 {
        bail!("parent block is 0; need at least one executed block in DB");
    }

    let parent_hash = factory
        .block_hash(parent_block)
        .with_context(|| format!("failed to fetch parent hash for block {parent_block}"))?
        .ok_or_else(|| eyre::eyre!("missing parent hash for block {parent_block}"))?;

    let parent_header = factory
        .header_by_number(parent_block)
        .with_context(|| format!("failed to fetch parent header for block {parent_block}"))?
        .ok_or_else(|| eyre::eyre!("missing parent header for block {parent_block}"))?;

    let target_header = factory
        .header_by_number(target_block)
        .with_context(|| format!("failed to fetch target header for block {target_block}"))?
        .ok_or_else(|| {
            eyre::eyre!(
                "target header block {} not found in local DB; \
                 this harness expects tip+1 to exist after unwind",
                target_block
            )
        })?;
    let canonical_root = target_header.state_root;

    let recovered_block = factory
        .recovered_block(target_block.into(), TransactionVariant::NoHash)
        .with_context(|| format!("failed to fetch recovered block {target_block}"))?
        .ok_or_else(|| {
            eyre::eyre!(
                "target block {} not found in local DB; \
                 ensure block bodies are present for tip+1",
                target_block
            )
        })?;

    let evm_config = EthEvmConfig::new(factory.chain_spec());
    let executor = evm_config.batch_executor(StateProviderDatabase(
        factory
            .history_by_block_hash(parent_hash)
            .with_context(|| {
                format!("failed to open state provider at parent hash {parent_hash:?}")
            })?,
    ));

    let execute_start = Instant::now();
    let execution = executor
        .execute(&recovered_block)
        .with_context(|| format!("failed to execute block {target_block}"))?;
    let execute_ms = elapsed_ms(execute_start);
    let outcome = execution.state;

    let reth_start = Instant::now();
    let reth_root = calculate_reth_root(factory, parent_hash, &outcome)
        .with_context(|| format!("failed to compute reth root for block {target_block}"))?;
    let reth_ms = elapsed_ms(reth_start);

    // v2/v_experimental fetchers validate an expected chain-tip hash to detect DB movement while reading.
    // For current-tip comparisons, this must be the actual DB tip hash (not parent hash).
    let expected_tip_block = factory
        .last_block_number()
        .context("failed to read last block number for sparse-trie consistency check")?;
    let expected_tip_hash = factory
        .block_hash(expected_tip_block)
        .with_context(|| format!("failed to fetch tip hash for block {expected_tip_block}"))?
        .ok_or_else(|| eyre::eyre!("missing tip hash for block {expected_tip_block}"))?;

    let shared_cache = SparseTrieSharedCache::new_with_parent_block_data(
        expected_tip_hash,
        parent_header.state_root,
    );

    let (v2_root, v2_ms, v2_fetched_nodes) = if skip_v2 {
        (None, None, None)
    } else {
        let mut local_v2 = SparseTrieLocalCache::default();
        let v2_start = Instant::now();
        let (v2_root_result, v2_metrics) = calculate_root_hash_with_sparse_trie(
            ConsistentDbView::new(factory.clone(), Some((parent_hash, parent_block))),
            &outcome,
            &[],
            &shared_cache,
            &mut local_v2,
            &None,
            ETHSpareMPTVersion::V2,
        );
        let v2_ms = elapsed_ms(v2_start);
        let v2_root = v2_root_result.with_context(|| "v2 root hash calculation failed")?;
        (Some(v2_root), Some(v2_ms), Some(v2_metrics.fetched_nodes))
    };

    let mut local_v_experimental = SparseTrieLocalCache::default();
    let v_experimental_start = Instant::now();
    let (v_experimental_root_result, v_experimental_metrics) = calculate_root_hash_with_sparse_trie(
        ConsistentDbView::new(factory.clone(), Some((parent_hash, parent_block))),
        &outcome,
        &[],
        &shared_cache,
        &mut local_v_experimental,
        &None,
        ETHSpareMPTVersion::VExperimental,
    );
    let v_experimental_ms = elapsed_ms(v_experimental_start);
    let v_experimental_root = v_experimental_root_result
        .with_context(|| "v_experimental root hash calculation failed")?;

    let (v1_root, v1_ms, v1_fetched_nodes) = if skip_v1 {
        (None, None, None)
    } else {
        let mut local_v1 = SparseTrieLocalCache::default();
        let v1_start = Instant::now();
        let (v1_root_result, v1_metrics) = calculate_root_hash_with_sparse_trie(
            ConsistentDbView::new(factory.clone(), Some((parent_hash, parent_block))),
            &outcome,
            &[],
            &shared_cache,
            &mut local_v1,
            &None,
            ETHSpareMPTVersion::V1,
        );
        let v1_ms = elapsed_ms(v1_start);
        let v1_root = v1_root_result.with_context(|| "v1 root hash calculation failed")?;
        (Some(v1_root), Some(v1_ms), Some(v1_metrics.fetched_nodes))
    };

    let v2_mismatch = v2_root.is_some_and(|root| root != canonical_root);
    let v1_mismatch = v1_root.is_some_and(|root| root != canonical_root);
    if v2_mismatch
        || v_experimental_root != canonical_root
        || reth_root != canonical_root
        || v1_mismatch
    {
        let v2_root_display = format_optional_root(v2_root.as_ref());
        let v1_root_display = format_optional_root(v1_root.as_ref());
        bail!(
            "root mismatch for block {}\n  canonical: {:?}\n  reth:      {:?}\n  v2:        {}\n  v_experimental: {:?}\n  v1:        {}",
            target_block,
            canonical_root,
            reth_root,
            v2_root_display,
            v_experimental_root,
            v1_root_display
        );
    }

    Ok(CompareResult {
        mode,
        parent_block,
        target_block,
        canonical_root,
        reth_root,
        v2_root,
        v_experimental_root,
        v1_root,
        execute_ms,
        reth_ms,
        v2_ms,
        v_experimental_ms,
        v1_ms,
        v2_fetched_nodes,
        v_experimental_fetched_nodes: v_experimental_metrics.fetched_nodes,
        v1_fetched_nodes,
    })
}

fn calculate_reth_root(
    factory: &Factory,
    parent_hash: B256,
    outcome: &BundleState,
) -> Result<B256> {
    let overlay = OverlayStateProviderFactory::new(factory.clone(), ChangesetCache::new());
    let hasher = factory
        .history_by_block_hash(parent_hash)
        .with_context(|| format!("failed to open state provider at parent hash {parent_hash:?}"))?;
    let hashed_post_state = hasher.hashed_post_state(outcome);
    let trie_input = TrieInput::from_state(hashed_post_state);
    ParallelStateRoot::new(overlay, trie_input.prefix_sets.freeze(), task_runtime())
        .incremental_root()
        .with_context(|| "parallel state root failed")
}

fn run_unwind_once(cli: &Cli, iteration: usize) -> Result<()> {
    let output = Command::new(&cli.reth_bin)
        .arg("stage")
        .arg("unwind")
        .arg(format!("--datadir={}", cli.datadir.display()))
        .arg(format!("--chain={}", cli.chain))
        .arg("num-blocks")
        .arg("1")
        .output()
        .with_context(|| {
            format!("failed to execute reth unwind command at iteration {iteration}")
        })?;

    if !output.status.success() {
        bail!(
            "reth unwind failed at iteration {} (status: {}):\nstdout:\n{}\nstderr:\n{}",
            iteration,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn open_provider_factory(
    datadir: &Path,
    chain: &str,
    static_file_blocks_per_file: Option<u64>,
) -> Result<Factory> {
    let db_path = datadir.join("db");
    let static_files_path = datadir.join("static_files");
    let chain_spec = parse_chain_spec(chain)?;

    let db = Arc::new(
        // Root computation can hold RO transactions long enough to hit MDBX timeout on large blocks.
        // Disable timeout for this offline harness path.
        open_db_read_only(
            &db_path,
            DatabaseArguments::new(ClientVersion::default())
                .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded)),
        )
        .with_context(|| format!("failed to open reth db at {}", db_path.display()))?,
    );

    let static_files = if let Some(blocks_per_file) = static_file_blocks_per_file {
        StaticFileProviderBuilder::read_only(&static_files_path)
            .with_blocks_per_file(blocks_per_file)
            .build()
    } else {
        StaticFileProvider::read_only(&static_files_path, false)
    }
    .with_context(|| {
        format!(
            "failed to open static files at {}",
            static_files_path.display()
        )
    })?;

    let rocksdb_provider = RocksDBProvider::builder(datadir.join("rocksdb"))
        .with_default_tables()
        .with_read_only(true)
        .build()
        .context("failed to open rocksdb provider")?;
    Ok(ProviderFactory::<NodeTypes>::new(
        db,
        chain_spec,
        static_files,
        rocksdb_provider,
        task_runtime(),
    )?)
}

/// Process-wide reth task runtime for parallel provider/trie I/O.
fn task_runtime() -> reth_tasks::Runtime {
    use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};

    static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            RuntimeBuilder::new(RuntimeConfig::default())
                .build()
                .expect("failed to build reth task runtime")
        })
        .clone()
}

fn parse_chain_spec(chain: &str) -> Result<Arc<ChainSpec>> {
    match chain.to_ascii_lowercase().as_str() {
        "mainnet" => Ok(MAINNET.clone()),
        "sepolia" => Ok(SEPOLIA.clone()),
        "holesky" => Ok(HOLESKY.clone()),
        "hoodi" => Ok(HOODI.clone()),
        "dev" => Ok(DEV.clone()),
        _ => bail!("unsupported chain: {chain} (supported: mainnet,sepolia,holesky,hoodi,dev)"),
    }
}

fn parse_cli() -> Result<Cli> {
    let mut full = false;
    let mut blocks: usize = 1;
    let mut reth_bin = expand_tilde("~/reth/target/maxperf/reth");
    let mut datadir = expand_tilde("/home/ubuntu/merkle/");
    let mut chain = "mainnet".to_string();
    let mut tip_fallback = false;
    let mut static_file_blocks_per_file = None;
    let mut skip_v2 = false;
    // v1 is disabled by default on modern DB layouts; opt in with --v1.
    let mut skip_v1 = true;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--full" => {
                full = true;
            }
            "--blocks" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--blocks requires a value"))?;
                blocks = value
                    .parse()
                    .with_context(|| format!("invalid --blocks value: {value}"))?;
            }
            "--reth-bin" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--reth-bin requires a value"))?;
                reth_bin = expand_tilde(&value);
            }
            "--datadir" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--datadir requires a value"))?;
                datadir = expand_tilde(&value);
            }
            "--chain" => {
                chain = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--chain requires a value"))?;
            }
            "--tip-fallback" => {
                tip_fallback = true;
            }
            "--static-file-blocks-per-file" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--static-file-blocks-per-file requires a value"))?;
                let parsed: u64 = value.parse().with_context(|| {
                    format!("invalid --static-file-blocks-per-file value: {value}")
                })?;
                if parsed == 0 {
                    bail!("--static-file-blocks-per-file must be > 0");
                }
                static_file_blocks_per_file = Some(parsed);
            }
            "--skip-v2" => {
                skip_v2 = true;
            }
            "--v1" => {
                skip_v1 = false;
            }
            "--skip-v1" => {
                skip_v1 = true;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => bail!("unknown arg: {arg}"),
        }
    }

    if blocks == 0 {
        bail!("--blocks must be > 0");
    }

    Ok(Cli {
        full,
        blocks,
        reth_bin,
        datadir,
        chain,
        tip_fallback,
        static_file_blocks_per_file,
        skip_v2,
        skip_v1,
    })
}

fn print_help() {
    println!("correctness-harness");
    println!("  default mode:");
    println!("    compares v1, v2, v_experimental and reth for block tip+1");
    println!("    expected setup: reth already unwound by 1 block");
    println!();
    println!("  --full");
    println!("    compares current tip, then unwinds by 1 per iteration");
    println!();
    println!("  flags:");
    println!("    --full                     run unwind+compare loop");
    println!("    --blocks <N>               iterations in --full mode (default: 1)");
    println!("    --reth-bin <PATH>          reth binary (default: ~/reth/target/maxperf/reth)");
    println!("    --datadir <PATH>           reth datadir (default: /home/ubuntu/merkle/)");
    println!("    --chain <NAME>             chain (default: mainnet)");
    println!("    --tip-fallback             compare current tip if tip+1 is unavailable");
    println!("    --static-file-blocks-per-file <N>");
    println!("                               override static file blocks-per-file lookup");
    println!("    --skip-v2                  skip v2 sparse trie comparison");
    println!("    --v1                       enable v1 sparse trie comparison (default: off)");
    println!("    --skip-v1                  skip v1 sparse trie comparison (default: on)");
}

fn print_compare_result(iteration: Option<usize>, result: &CompareResult) {
    if let Some(iteration) = iteration {
        println!("iteration {iteration}:");
    }

    println!(
        "  mode={} parent={} target={} status=ok",
        match result.mode {
            CompareMode::TipPlusOne => "tip+1",
            CompareMode::TipFallback => "tip-fallback",
            CompareMode::CurrentTip => "tip",
        },
        result.parent_block,
        result.target_block
    );
    println!("  roots:");
    println!("    canonical: {:?}", result.canonical_root);
    println!(
        "    reth:      {:?} {}",
        result.reth_root,
        root_match_marker(result.reth_root == result.canonical_root)
    );
    println!(
        "    v2:        {} {}",
        format_optional_root(result.v2_root.as_ref()),
        optional_root_match_marker(result.v2_root.as_ref(), &result.canonical_root)
    );
    println!(
        "    v_experimental: {:?} {}",
        result.v_experimental_root,
        root_match_marker(result.v_experimental_root == result.canonical_root)
    );
    println!(
        "    v1:        {} {}",
        format_optional_root(result.v1_root.as_ref()),
        optional_root_match_marker(result.v1_root.as_ref(), &result.canonical_root)
    );
    let v2_ms = result
        .v2_ms
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "skipped".to_string());
    let v1_ms = result
        .v1_ms
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "skipped".to_string());
    println!(
        "  time_ms: execute={:.2} reth={:.2} v2={} v_experimental={:.2} v1={}",
        result.execute_ms, result.reth_ms, v2_ms, result.v_experimental_ms, v1_ms
    );
    let v2_nodes = result
        .v2_fetched_nodes
        .map(|v| v.to_string())
        .unwrap_or_else(|| "skipped".to_string());
    let v1_nodes = result
        .v1_fetched_nodes
        .map(|v| v.to_string())
        .unwrap_or_else(|| "skipped".to_string());
    println!(
        "  fetched_nodes: v2={} v_experimental={} v1={}",
        v2_nodes, result.v_experimental_fetched_nodes, v1_nodes
    );
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn format_optional_root(root: Option<&B256>) -> String {
    root.map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "None".to_string())
}

fn root_match_marker(matches: bool) -> &'static str {
    if matches {
        "(= canonical)"
    } else {
        "(!= canonical)"
    }
}

fn optional_root_match_marker(root: Option<&B256>, canonical_root: &B256) -> &'static str {
    match root {
        Some(value) if value == canonical_root => "(= canonical)",
        Some(_) => "(!= canonical)",
        None => "(skipped)",
    }
}
