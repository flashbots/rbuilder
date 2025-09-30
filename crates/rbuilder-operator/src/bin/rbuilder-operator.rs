use rbuilder::live_builder::cli::{self};
use rbuilder_operator::{
    build_info::{print_version_info, rbuilder_version},
    flashbots_config::FlashbotsConfig,
};
use tracing::info;

fn on_run() {
    info!(version = ?rbuilder_version(), "Flashbots rbuilder version");
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    return cli::run::<FlashbotsConfig>(print_version_info, Some(on_run)).await;
}
