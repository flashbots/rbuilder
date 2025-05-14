//! CLI tool to validate a rbuilder config file

use clap::Parser;
use rbuilder::live_builder::{base_config::load_config_toml_and_env, config::Config};
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config_path = &cli.config;
    let _: Config = load_config_toml_and_env(config_path)?;

    println!("Config file '{}' is valid", config_path.display());

    Ok(())
}
