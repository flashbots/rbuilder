use eyre::{bail, Context, Result};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
struct Cli {
    correctness_bin: PathBuf,
    cache_warm_bin: PathBuf,
    cache_warm_iters: usize,
    cache_warm_keys: usize,
    cache_warm_percentages: String,
    cache_warm_out_csv: Option<PathBuf>,
    correctness_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct FullMode {
    blocks: usize,
    reth_bin: PathBuf,
    datadir: PathBuf,
    chain: String,
}

fn main() -> Result<()> {
    let cli = parse_cli()?;

    if let Some(full_mode) = parse_full_mode(&cli.correctness_args)? {
        run_full_interleaved(&cli, &full_mode)?;
    } else {
        run_correctness(&cli)?;
        run_cache_warm(&cli, false, Some(1), true)?;
    }

    Ok(())
}

fn run_correctness(cli: &Cli) -> Result<()> {
    run_correctness_with_args(cli, &cli.correctness_args)
}

fn run_correctness_with_args(cli: &Cli, args: &[String]) -> Result<()> {
    println!(
        "running correctness harness: {} {}",
        cli.correctness_bin.display(),
        args.join(" ")
    );

    let status = Command::new(&cli.correctness_bin)
        .args(args)
        .status()
        .with_context(|| {
            format!(
                "failed to start correctness harness at {}; build binaries with `cargo build --release --bins` or pass --correctness-bin",
                cli.correctness_bin.display()
            )
        })?;

    if !status.success() {
        bail!("correctness harness failed with status {status}");
    }

    Ok(())
}

fn run_cache_warm(
    cli: &Cli,
    append_csv: bool,
    csv_iteration: Option<usize>,
    v3_root_correct: bool,
) -> Result<()> {
    println!();
    let mut summary = format!(
        "running cache-warm benchmark: {} --iters {} --keys {} --percentages {}",
        cli.cache_warm_bin.display(),
        cli.cache_warm_iters,
        cli.cache_warm_keys,
        cli.cache_warm_percentages
    );
    if let Some(out_csv) = &cli.cache_warm_out_csv {
        summary.push_str(&format!(" --out-csv {}", out_csv.display()));
        if append_csv {
            summary.push_str(" --append-csv");
        }
        if let Some(iteration) = csv_iteration {
            summary.push_str(&format!(" --csv-iteration {iteration}"));
            summary.push_str(&format!(" --v3-root-correct {}", v3_root_correct));
        }
    }
    println!("{summary}");

    let mut cmd = Command::new(&cli.cache_warm_bin);
    cmd.arg("--iters")
        .arg(cli.cache_warm_iters.to_string())
        .arg("--keys")
        .arg(cli.cache_warm_keys.to_string())
        .arg("--percentages")
        .arg(&cli.cache_warm_percentages);
    if let Some(out_csv) = &cli.cache_warm_out_csv {
        cmd.arg("--out-csv").arg(out_csv);
        if append_csv {
            cmd.arg("--append-csv");
        }
        if let Some(iteration) = csv_iteration {
            cmd.arg("--csv-iteration").arg(iteration.to_string());
            cmd.arg("--v3-root-correct")
                .arg(if v3_root_correct { "true" } else { "false" });
        }
    }

    let status = cmd
        .status()
        .with_context(|| {
            format!(
                "failed to start cache-warm benchmark at {}; build binaries with `cargo build --release --bins` or pass --cache-warm-bin",
                cli.cache_warm_bin.display()
            )
        })?;

    if !status.success() {
        bail!("cache-warm benchmark failed with status {status}");
    }

    Ok(())
}

fn run_full_interleaved(cli: &Cli, full_mode: &FullMode) -> Result<()> {
    let single_iteration_args = rewrite_to_single_full_iteration(&cli.correctness_args)?;
    let append_csv = full_mode.blocks > 1 && cli.cache_warm_out_csv.is_some();

    println!(
        "running full interleaved mode: blocks={} (correctness -> cache-warm -> unwind)",
        full_mode.blocks
    );

    for i in 0..full_mode.blocks {
        println!();
        println!("interleaved iteration {}/{}", i + 1, full_mode.blocks);
        run_correctness_with_args(cli, &single_iteration_args)?;
        run_cache_warm(cli, append_csv, Some(i + 1), true)?;
        if i + 1 < full_mode.blocks {
            run_unwind_once(full_mode, i + 1)?;
        }
    }

    println!(
        "full interleaved run passed for {} iterations",
        full_mode.blocks
    );
    Ok(())
}

fn run_unwind_once(full_mode: &FullMode, iteration: usize) -> Result<()> {
    let output = Command::new(&full_mode.reth_bin)
        .arg("stage")
        .arg("unwind")
        .arg(format!("--datadir={}", full_mode.datadir.display()))
        .arg(format!("--chain={}", full_mode.chain))
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

fn parse_full_mode(correctness_args: &[String]) -> Result<Option<FullMode>> {
    let mut full = false;
    let mut blocks = 1usize;
    let mut reth_bin = expand_tilde("~/reth/target/maxperf/reth");
    let mut datadir = expand_tilde("/home/ubuntu/merkle/");
    let mut chain = "mainnet".to_string();

    let mut i = 0usize;
    while i < correctness_args.len() {
        match correctness_args[i].as_str() {
            "--full" => {
                full = true;
                i += 1;
            }
            "--blocks" => {
                let value = correctness_args
                    .get(i + 1)
                    .ok_or_else(|| eyre::eyre!("--blocks requires a value"))?;
                blocks = value
                    .parse()
                    .with_context(|| format!("invalid --blocks value: {value}"))?;
                i += 2;
            }
            "--reth-bin" => {
                let value = correctness_args
                    .get(i + 1)
                    .ok_or_else(|| eyre::eyre!("--reth-bin requires a value"))?;
                reth_bin = expand_tilde(value);
                i += 2;
            }
            "--datadir" => {
                let value = correctness_args
                    .get(i + 1)
                    .ok_or_else(|| eyre::eyre!("--datadir requires a value"))?;
                datadir = expand_tilde(value);
                i += 2;
            }
            "--chain" => {
                let value = correctness_args
                    .get(i + 1)
                    .ok_or_else(|| eyre::eyre!("--chain requires a value"))?;
                chain = value.clone();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    if !full {
        return Ok(None);
    }
    if blocks == 0 {
        bail!("--blocks must be > 0");
    }

    Ok(Some(FullMode {
        blocks,
        reth_bin,
        datadir,
        chain,
    }))
}

fn rewrite_to_single_full_iteration(correctness_args: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(correctness_args.len() + 2);
    let mut saw_full = false;
    let mut i = 0usize;

    while i < correctness_args.len() {
        match correctness_args[i].as_str() {
            "--full" => {
                saw_full = true;
                out.push(correctness_args[i].clone());
                i += 1;
            }
            "--blocks" => {
                if i + 1 >= correctness_args.len() {
                    bail!("--blocks requires a value");
                }
                i += 2;
            }
            _ => {
                out.push(correctness_args[i].clone());
                i += 1;
            }
        }
    }

    if !saw_full {
        out.push("--full".to_string());
    }
    out.push("--blocks".to_string());
    out.push("1".to_string());
    Ok(out)
}

fn parse_cli() -> Result<Cli> {
    let mut correctness_bin = sibling_binary("correctness-harness");
    let mut cache_warm_bin = sibling_binary("cache-warm-compare");
    let mut cache_warm_iters = 20usize;
    let mut cache_warm_keys = 10_000usize;
    let mut cache_warm_percentages = "70,80,90,100".to_string();
    let mut cache_warm_out_csv = None;
    let mut correctness_args = Vec::new();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--correctness-bin" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--correctness-bin requires a value"))?;
                correctness_bin = expand_tilde(&value);
            }
            "--cache-warm-bin" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--cache-warm-bin requires a value"))?;
                cache_warm_bin = expand_tilde(&value);
            }
            "--cache-warm-iters" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--cache-warm-iters requires a value"))?;
                cache_warm_iters = value
                    .parse()
                    .with_context(|| format!("invalid --cache-warm-iters value: {value}"))?;
            }
            "--cache-warm-keys" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--cache-warm-keys requires a value"))?;
                cache_warm_keys = value
                    .parse()
                    .with_context(|| format!("invalid --cache-warm-keys value: {value}"))?;
            }
            "--cache-warm-percentages" => {
                cache_warm_percentages = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--cache-warm-percentages requires a value"))?;
            }
            "--cache-warm-out-csv" => {
                let value = args
                    .next()
                    .ok_or_else(|| eyre::eyre!("--cache-warm-out-csv requires a value"))?;
                cache_warm_out_csv = Some(expand_tilde(&value));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => correctness_args.push(arg),
        }
    }

    if cache_warm_iters == 0 {
        bail!("--cache-warm-iters must be > 0");
    }
    if cache_warm_keys == 0 {
        bail!("--cache-warm-keys must be > 0");
    }

    Ok(Cli {
        correctness_bin,
        cache_warm_bin,
        cache_warm_iters,
        cache_warm_keys,
        cache_warm_percentages,
        cache_warm_out_csv,
        correctness_args,
    })
}

fn print_help() {
    println!("correct_and_bench");
    println!("  Runs correctness-harness and cache-warm-compare.");
    println!("  In --full mode, runs cache-warm after each correctness iteration.");
    println!();
    println!("  wrapper flags:");
    println!("    --correctness-bin <PATH>   correctness-harness binary path");
    println!("    --cache-warm-bin <PATH>    cache-warm-compare binary path");
    println!("    --cache-warm-iters <N>     benchmark iterations (default: 20)");
    println!("    --cache-warm-keys <N>      benchmark keys (default: 10000)");
    println!("    --cache-warm-percentages <CSV>");
    println!("                               benchmark warm percentages (default: 70,80,90,100)");
    println!("    --cache-warm-out-csv <PATH> write cache-warm result rows to CSV");
    println!();
    println!("  all other flags are passed through to correctness-harness.");
    println!();
    println!("  example:");
    println!(
        "    correct_and_bench --datadir /mnt/reth-archive --chain mainnet --tip-fallback --cache-warm-iters 10"
    );
}

fn sibling_binary(name: &str) -> PathBuf {
    let filename = format!("{name}{}", env::consts::EXE_SUFFIX);
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| dir.join(&filename))
        .unwrap_or_else(|| PathBuf::from(filename))
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}
