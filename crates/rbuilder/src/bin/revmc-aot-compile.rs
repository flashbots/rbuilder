use rbuilder::live_builder::{base_config, config::Config as ConfigType};
use revmc_toolkit_utils::{evm as evm_utils, build as build_utils};
use revmc_toolkit_sim::gas_guzzlers::GasGuzzlerConfig;
use revmc_toolkit_build::CompilerOptions;
use std::{path::PathBuf, str::FromStr};
use revm::primitives::SpecId;
use eyre::{OptionExt, Result};
use tracing::debug;
use clap::Parser;

// todo: compiler config in file instead?
#[derive(Parser, Debug)]
struct Cli {
    #[clap(flatten)]
    compiler_opt: CompilerOptionsCli,
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config_path: PathBuf,
    #[clap(subcommand)]
    commands: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    #[clap(about = "Fetches contracts specified in config file.")]
    FromConfig { build_file_path: PathBuf },
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

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let Cli { compiler_opt, config_path, commands } = Cli::parse();
    let provider_factory = new_provider_factory(config_path)?;

    match commands {
        Commands::FromConfig { build_file_path } => {
            debug!("Fetching contracts from config at {:?}", build_file_path);
            let _: () = build_utils::compile_aot_from_file_path(
                &provider_factory.latest()?, 
                &build_file_path
            )?.into_iter().collect::<Result<_>>()?;
        }
        Commands::GasGuzzlers(gas_guzzlers_config) => {
            debug!("Searching for gas guzzlers");
            let head_size = gas_guzzlers_config.head_size;
            let bytecodes = GasGuzzlerConfig::from(gas_guzzlers_config)
                .find_gas_guzzlers(provider_factory)?
                .contract_to_bytecode()?
                .into_top_guzzlers(head_size);
            let compiler_opt = CompilerOptions::try_from(compiler_opt)?;
            debug!("Compiling contracts to {:?}",  compiler_opt.out_dir);
            let _: () = build_utils::compile_aot_from_codes(
                bytecodes, 
                Some(compiler_opt)
            )?.into_iter().collect::<Result<_>>()?;
        }
        // todo: add remove command
        // todo: add command that lists top gas guzzler addresses
    }
         
    Ok(())
}

fn new_provider_factory(config_path: PathBuf) -> Result<evm_utils::ProviderFactory<evm_utils::DatabaseEnv>> {
    let config: ConfigType = base_config::load_config_toml_and_env(config_path)?;
    // todo: get provider_factory straight from the config with create_provider_factory
    let reth_path = config.base_config.reth_db_path.expect("RETH_DB_PATH field not set");
    evm_utils::make_provider_factory(&reth_path)
}