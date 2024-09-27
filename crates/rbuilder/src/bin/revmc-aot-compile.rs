

use clap::Parser;
use revmc_toolbox_sim::gas_guzzlers::GasGuzzlerConfig;
use revmc_toolbox_build::{compile_contracts_aot, CompilerOptions, OptimizationLevelDeseralizable};
use revm::primitives::SpecId;
use std::{path::PathBuf, str::FromStr};
use eyre::{OptionExt, Result};
use std::sync::Arc;
use revmc_toolbox_utils::evm as evm_utils;
use tracing::debug;

// todo: compiler opt to file

#[derive(Parser, Debug)]
struct Cli {
    #[clap(flatten)]
    compiler_opt: CompilerOptionsCli,
    #[clap(subcommand)]
    commands: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    #[clap(about = "Fetches contracts specified in config file.")]
    FromConfig,
    #[clap(about = "Fetches contracts based on their historical gas usage.")]
    GasGuzzlers(GasGuzzlerConfigCli),
}

#[derive(Parser, Debug)]
struct CompilerOptionsCli {
    #[clap(long, value_parser, help = "Output directory")]
    out_dir: Option<String>,
    #[clap(long, value_parser, help = "Target features")]
    target_features: Option<String>,
    #[clap(long, value_parser, help = "Target CPU")]
    target_cpu: Option<String>,
    #[clap(long, value_parser, help = "Target")]
    target: Option<String>,
    #[clap(long, value_parser, help = "Optimization level")]
    opt_level: Option<u8>,
    #[clap(long, value_parser, help = "No link")]
    no_link: Option<bool>,
    #[clap(long, value_parser, help = "No gas")]
    no_gas: Option<bool>,
    #[clap(long, value_parser, help = "No length checks")]
    no_len_checks: Option<bool>,
    #[clap(long, value_parser, help = "Frame pointers")]
    frame_pointers: Option<bool>,
    #[clap(long, value_parser, help = "Debug assertions")]
    debug_assertions: Option<bool>,
    #[clap(long, value_parser, help = "Label")]
    label: Option<String>,
    #[clap(long, value_parser, help = "Spec id")]
    spec_id: Option<u8>,
}


macro_rules! set_if_some {
    ($src:ident, $dst:ident, { $( $field:ident ),+ }) => {
        $(
            if let Some(value) = $src.$field {
                $dst.$field = value;
            }
        )+
    };
}

macro_rules! set_if_some_opt {
    ($src:ident, $dst:ident, { $( $field:ident ),+ }) => {
        $(
            if let Some(value) = $src.$field {
                $dst.$field = Some(value);
            }
        )+
    };
}

impl TryFrom<CompilerOptionsCli> for CompilerOptions {
    type Error = eyre::Error;

    fn try_from(config_cli: CompilerOptionsCli) -> Result<Self, Self::Error> {
        let mut config = CompilerOptions::default();

        set_if_some_opt!(config_cli, config, { target_features, target_cpu, label });
        set_if_some!(config_cli, config, { no_link, no_gas, no_len_checks, frame_pointers, debug_assertions, target });

        if let Some(out_dir) = config_cli.out_dir {
            config.out_dir = PathBuf::from_str(&out_dir)?;
        }
        config.opt_level = config_cli.opt_level
            .map(|opt_id| opt_id.try_into())
            .transpose()?
            .unwrap_or_default();
        config.spec_id = config_cli.spec_id
            .map(|s_id| SpecId::try_from_u8(s_id).ok_or_eyre("Invalid spec id"))
            .transpose()?
            .unwrap_or_default();

        Ok(config)
    }
}


#[derive(Parser, Debug)]
struct GasGuzzlerConfigCli {
    #[clap(long, help = "Start block")]
    start_block: u64,
    #[clap(long, help = "End block")]
    end_block: Option<u64>,
    #[clap(long, help = "Number of top contracts to consider.")]
    head_size: usize,
    #[clap(long, help = "Size of the sample. If not provided, all blocks will be used.")]
    sample_size: Option<u64>,
    #[clap(long, help = "Seed for generating pseudo-random sample")]
    seed: Option<String>,

}

impl From<GasGuzzlerConfigCli> for GasGuzzlerConfig {
    fn from(config: GasGuzzlerConfigCli) -> Self {
        let rnd_seed = config.seed.map(|seed| revm::primitives::keccak256(seed.as_bytes()).0);
        Self {
            start_block: config.start_block,
            end_block: config.end_block,
            sample_size: config.sample_size,
            seed: rnd_seed,
        }
    }
}


fn main() -> Result<()> {
    let Cli { compiler_opt, commands } = Cli::parse();

    let bytecodes = match commands {
        Commands::FromConfig => {
            debug!("Fetching contracts from config");
            unimplemented!() // TODO
        }
        Commands::GasGuzzlers(gas_guzzlers_config) => {
            debug!("Searching for gas guzzlers");
            println!("Gas guzzlers: {:?}", gas_guzzlers_config);
            let provider_factory = new_provider_factory()?;
            let head_size = gas_guzzlers_config.head_size;
            // todo: improve this to eliminate contracts that dont have conistent gas usage
            GasGuzzlerConfig::from(gas_guzzlers_config)
                .find_gas_guzzlers(provider_factory)?
                .contract_to_bytecode()?
                .into_top_guzzlers(head_size)
        }
        // todo: add remove command
        // todo: add command that lists top gas guzzler addresses
    };
    let bytecodes = bytecodes.into_iter().map(|c| c.into()).collect();
    let compiler_opt = CompilerOptions::try_from(compiler_opt)?;
    let out_dir = compiler_opt.out_dir.clone();
    debug!("Compiling contracts");
    compile_contracts_aot(bytecodes, Some(compiler_opt))?
        .into_iter().collect::<Result<_>>()?;
    debug!("Compiled contracts have been written to {out_dir:?}");
         
    Ok(())
}

fn new_provider_factory() -> Result<Arc<evm_utils::ProviderFactory<evm_utils::DatabaseEnv>>> {
    // todo: source this from toml config file instead (like the rest of the program)
    dotenv::dotenv()?;
    let dir_path = std::env::var("RETH_DB_PATH")
        .expect("RETH_DB_PATH env var not set");
    let dir_path = PathBuf::from_str(&dir_path)?;
    evm_utils::make_provider_factory(&dir_path).map(Arc::new)
}