//! Instantiation of run_backtest_build_block on our sample configuration.

use rbuilder::{backtest::run_backtest_build_block, live_builder::config::Config};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest_build_block::<Config>().await
}
