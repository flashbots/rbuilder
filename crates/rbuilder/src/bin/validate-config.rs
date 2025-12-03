//! CLI tool to validate a rbuilder config file

use clap::Parser;
use rbuilder::live_builder::config::Config;
use rbuilder_config::load_toml_config;
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
    let _: Config = load_toml_config(config_path)?;

    println!("Config file '{}' is valid", config_path.display());

    Ok(())
}
