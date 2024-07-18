//! Instantiation of run_backtest_build_range on our sample configuration.
use rbuilder::{backtest::run_backtest_build_range, live_builder::config::Config};

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    run_backtest_build_range::<Config>().await
}
