use alloy_primitives::{keccak256, B256, U256};
use eth_sparse_mpt::{
    v2::trie::{proof_store::ProofStore as ProofStoreV2, Trie as TrieV2},
    v_experimental::trie::{
        proof_store::ProofStore as ProofStoreVExperimental, Trie as TrieVExperimental,
    },
};
use eyre::{bail, Context, Result};
use std::{
    env,
    fs::{create_dir_all, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
struct Cli {
    iters: usize,
    keys: usize,
    percentages: Vec<usize>,
    out_csv: Option<PathBuf>,
    append_csv: bool,
    csv_iteration: Option<usize>,
    v_experimental_root_correct: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Stats {
    median_ms: f64,
    p99_ms: f64,
    mean_ms: f64,
}

fn main() -> Result<()> {
    let cli = parse_cli()?;
    let (keys, values) = prepare_key_value_data(cli.keys);
    let empty_proof_store_v2 = ProofStoreV2::default();
    let empty_proof_store_v_experimental = ProofStoreVExperimental::default();
    let include_correctness_columns =
        cli.csv_iteration.is_some() || cli.v_experimental_root_correct.is_some();
    let mut csv_writer = open_csv_writer(
        cli.out_csv.as_deref(),
        cli.append_csv,
        include_correctness_columns,
    )?;

    println!(
        "Running cache-warm comparison with iters={}, keys={}, percentages={:?}",
        cli.iters, cli.keys, cli.percentages
    );
    println!(
        "baseline=v2::Trie(root_hash parallel=true), optimized=v_experimental::Trie(root_hash parallel=true)"
    );
    println!();
    println!(
        "{:<8} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "warm_%",
        "v2_p50",
        "v_experimental_p50",
        "v2_p99",
        "v_experimental_p99",
        "v2_mean",
        "v_experimental_mean",
        "speedup"
    );
    if let Some((writer, write_header, include_correctness_columns)) = csv_writer.as_mut() {
        if *write_header {
            if *include_correctness_columns {
                writeln!(
                    writer,
                    "iteration,v_experimental_root_correct,warm_pct,v2_p50_ms,v_experimental_p50_ms,v2_p99_ms,v_experimental_p99_ms,v2_mean_ms,v_experimental_mean_ms,speedup"
                )
                .context("failed to write CSV header")?;
            } else {
                writeln!(
                    writer,
                    "warm_pct,v2_p50_ms,v_experimental_p50_ms,v2_p99_ms,v_experimental_p99_ms,v2_mean_ms,v_experimental_mean_ms,speedup"
                )
                .context("failed to write CSV header")?;
            }
            writer.flush().context("failed to flush CSV header")?;
        }
    }

    for pct in &cli.percentages {
        let prewarm = keys.len() * *pct / 100;

        let v2_template = make_v2_template(&keys, &values, prewarm, &empty_proof_store_v2)?;
        let v_experimental_template = make_v_experimental_template(
            &keys,
            &values,
            prewarm,
            &empty_proof_store_v_experimental,
        )?;

        let mut v2_times = Vec::with_capacity(cli.iters);
        let mut v_experimental_times = Vec::with_capacity(cli.iters);

        for _ in 0..cli.iters {
            let (v2_hash, v2_elapsed) = run_v2_once(
                v2_template.clone(),
                &keys,
                &values,
                prewarm,
                &empty_proof_store_v2,
            )?;
            let (v_experimental_hash, v_experimental_elapsed) = run_v_experimental_once(
                v_experimental_template.clone(),
                &keys,
                &values,
                prewarm,
                &empty_proof_store_v_experimental,
            )?;

            if v2_hash != v_experimental_hash {
                bail!(
                    "root hash mismatch at warm {}%: v2={v2_hash:?} v_experimental={v_experimental_hash:?}",
                    pct
                );
            }

            v2_times.push(v2_elapsed);
            v_experimental_times.push(v_experimental_elapsed);
        }

        let v2_stats = calc_stats(&v2_times)?;
        let v_experimental_stats = calc_stats(&v_experimental_times)?;
        let speedup = if v_experimental_stats.mean_ms > 0.0 {
            v2_stats.mean_ms / v_experimental_stats.mean_ms
        } else {
            f64::INFINITY
        };

        println!(
            "{:<8} {:>10.3}ms {:>10.3}ms {:>10.3}ms {:>10.3}ms {:>10.3}ms {:>10.3}ms {:>10.2}x",
            pct,
            v2_stats.median_ms,
            v_experimental_stats.median_ms,
            v2_stats.p99_ms,
            v_experimental_stats.p99_ms,
            v2_stats.mean_ms,
            v_experimental_stats.mean_ms,
            speedup
        );

        if let Some((writer, _, include_correctness_columns)) = csv_writer.as_mut() {
            if *include_correctness_columns {
                let iteration = cli.csv_iteration.map(|v| v.to_string()).unwrap_or_default();
                let v_experimental_root_correct = cli
                    .v_experimental_root_correct
                    .map(|v| if v { "true" } else { "false" })
                    .unwrap_or("");
                writeln!(
                    writer,
                    "{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
                    iteration,
                    v_experimental_root_correct,
                    pct,
                    v2_stats.median_ms,
                    v_experimental_stats.median_ms,
                    v2_stats.p99_ms,
                    v_experimental_stats.p99_ms,
                    v2_stats.mean_ms,
                    v_experimental_stats.mean_ms,
                    speedup
                )
                .context("failed to write CSV row")?;
            } else {
                writeln!(
                    writer,
                    "{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
                    pct,
                    v2_stats.median_ms,
                    v_experimental_stats.median_ms,
                    v2_stats.p99_ms,
                    v_experimental_stats.p99_ms,
                    v2_stats.mean_ms,
                    v_experimental_stats.mean_ms,
                    speedup
                )
                .context("failed to write CSV row")?;
            }
            writer.flush().context("failed to flush CSV row")?;
        }
    }

    if let Some((writer, _, _)) = csv_writer.as_mut() {
        writer.flush().context("failed to flush CSV writer")?;
    }
    if let Some(path) = &cli.out_csv {
        println!("wrote cache-warm CSV: {}", path.display());
    }

    Ok(())
}

fn parse_cli() -> Result<Cli> {
    let mut iters = 20usize;
    let mut keys = 10_000usize;
    let mut percentages = vec![70usize, 80, 90, 100];
    let mut out_csv = None;
    let mut append_csv = false;
    let mut csv_iteration = None;
    let mut v_experimental_root_correct = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--iters" => {
                let v = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--iters requires a value"))?;
                iters = v.parse().with_context(|| format!("invalid --iters: {v}"))?;
            }
            "--keys" => {
                let v = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--keys requires a value"))?;
                keys = v.parse().with_context(|| format!("invalid --keys: {v}"))?;
            }
            "--percentages" => {
                let v = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--percentages requires a value"))?;
                percentages = parse_percentages(&v)?;
            }
            "--out-csv" => {
                let v = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--out-csv requires a value"))?;
                out_csv = Some(expand_tilde(&v));
            }
            "--append-csv" => {
                append_csv = true;
            }
            "--csv-iteration" => {
                let v = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--csv-iteration requires a value"))?;
                let parsed: usize = v
                    .parse()
                    .with_context(|| format!("invalid --csv-iteration: {v}"))?;
                csv_iteration = Some(parsed);
            }
            "--v-experimental-root-correct" | "--v3-root-correct" => {
                let v = args
                    .next()
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "--v-experimental-root-correct requires a value (or use legacy --v3-root-correct)"
                        )
                    })?;
                let parsed: bool = v
                    .parse()
                    .with_context(|| format!("invalid correctness flag value: {v}"))?;
                v_experimental_root_correct = Some(parsed);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => bail!("unknown arg: {arg}"),
        }
    }

    if iters == 0 {
        bail!("--iters must be > 0");
    }
    if keys == 0 {
        bail!("--keys must be > 0");
    }
    if percentages.is_empty() {
        bail!("--percentages must not be empty");
    }
    for p in &percentages {
        if *p > 100 {
            bail!("percentage out of range [0,100]: {p}");
        }
    }

    Ok(Cli {
        iters,
        keys,
        percentages,
        out_csv,
        append_csv,
        csv_iteration,
        v_experimental_root_correct,
    })
}

fn parse_percentages(input: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for p in input.split(',') {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        out.push(
            p.parse()
                .with_context(|| format!("invalid percentage: {p}"))?,
        );
    }
    Ok(out)
}

fn print_help() {
    println!("cache-warm-compare");
    println!("  --iters <N>              iterations per percentage (default: 20)");
    println!("  --keys <N>               number of key/value pairs (default: 10000)");
    println!("  --percentages <CSV>      pre-warm percentages (default: 70,80,90,100)");
    println!("  --out-csv <PATH>         write results to CSV");
    println!("  --append-csv             append rows to CSV instead of truncating");
    println!("  --csv-iteration <N>      iteration id to include in CSV rows");
    println!(
        "  --v-experimental-root-correct <BOOL> include v_experimental correctness (true/false) in CSV rows"
    );
    println!("  --v3-root-correct <BOOL> legacy alias for --v-experimental-root-correct");
}

fn open_csv_writer(
    path: Option<&Path>,
    append: bool,
    include_correctness_columns: bool,
) -> Result<Option<(BufWriter<File>, bool, bool)>> {
    let Some(path) = path else {
        return Ok(None);
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent).with_context(|| {
            format!(
                "failed to create parent directory for CSV: {}",
                parent.display()
            )
        })?;
    }

    if append {
        let existed = path.exists();
        let existing_len = if existed {
            Some(
                path.metadata()
                    .with_context(|| format!("failed to stat CSV file {}", path.display()))?
                    .len(),
            )
        } else {
            None
        };
        let mut effective_include_correctness_columns = include_correctness_columns;
        if let Some(len) = existing_len {
            if len > 0 {
                let mut first_line = String::new();
                BufReader::new(
                    File::open(path)
                        .with_context(|| format!("failed to read CSV file {}", path.display()))?,
                )
                .read_line(&mut first_line)
                .with_context(|| format!("failed to read CSV header {}", path.display()))?;
                let header_has_correctness_column = first_line
                    .trim_end()
                    .split(',')
                    .any(|h| h == "v_experimental_root_correct" || h == "v3_root_correct");
                if include_correctness_columns && !header_has_correctness_column {
                    bail!(
                        "existing CSV {} does not include v_experimental_root_correct (or legacy v3_root_correct) column; use a new --out-csv path",
                        path.display()
                    );
                }
                effective_include_correctness_columns = header_has_correctness_column;
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open CSV file {}", path.display()))?;
        let write_header = !existed || existing_len.is_some_and(|len| len == 0);
        Ok(Some((
            BufWriter::new(file),
            write_header,
            effective_include_correctness_columns,
        )))
    } else {
        let file = File::create(path)
            .with_context(|| format!("failed to create CSV file {}", path.display()))?;
        Ok(Some((
            BufWriter::new(file),
            true,
            include_correctness_columns,
        )))
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn prepare_key_value_data(n: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut keys = Vec::with_capacity(n);
    let mut values = Vec::with_capacity(n);
    for i in 0u64..(n as u64) {
        let b: B256 = U256::from(i).into();
        let key = keccak256(b).to_vec();
        let value = keccak256(&key).to_vec();
        keys.push(key);
        values.push(value);
    }
    (keys, values)
}

fn make_v2_template(
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
    prewarm: usize,
    proof_store: &ProofStoreV2,
) -> Result<TrieV2> {
    let mut trie = TrieV2::new_empty();
    for i in 0..prewarm {
        trie.insert(&keys[i], &values[i])?;
    }
    if prewarm > 0 {
        let _ = trie.root_hash(true, proof_store)?;
    }
    Ok(trie)
}

fn make_v_experimental_template(
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
    prewarm: usize,
    proof_store: &ProofStoreVExperimental,
) -> Result<TrieVExperimental> {
    let mut trie = TrieVExperimental::new_empty();
    for i in 0..prewarm {
        trie.insert(&keys[i], &values[i])?;
    }
    if prewarm > 0 {
        let _ = trie.root_hash(true, proof_store)?;
    }
    Ok(trie)
}

fn run_v2_once(
    mut trie: TrieV2,
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
    start: usize,
    proof_store: &ProofStoreV2,
) -> Result<(B256, Duration)> {
    let start_time = Instant::now();
    for i in start..keys.len() {
        trie.insert(&keys[i], &values[i])?;
    }
    let hash = trie.root_hash(true, proof_store)?;
    Ok((hash, start_time.elapsed()))
}

fn run_v_experimental_once(
    mut trie: TrieVExperimental,
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
    start: usize,
    proof_store: &ProofStoreVExperimental,
) -> Result<(B256, Duration)> {
    let start_time = Instant::now();
    for i in start..keys.len() {
        trie.insert(&keys[i], &values[i])?;
    }
    let hash = trie.root_hash(true, proof_store)?;
    Ok((hash, start_time.elapsed()))
}

fn calc_stats(data: &[Duration]) -> Result<Stats> {
    if data.is_empty() {
        bail!("no data points");
    }
    let mut ms: Vec<f64> = data.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.total_cmp(b));

    let mean_ms = ms.iter().sum::<f64>() / ms.len() as f64;
    let median_ms = if ms.len() % 2 == 0 {
        (ms[ms.len() / 2 - 1] + ms[ms.len() / 2]) / 2.0
    } else {
        ms[ms.len() / 2]
    };
    let p99_idx = (ms.len() * 99 / 100).min(ms.len() - 1);
    let p99_ms = ms[p99_idx];

    Ok(Stats {
        median_ms,
        p99_ms,
        mean_ms,
    })
}
