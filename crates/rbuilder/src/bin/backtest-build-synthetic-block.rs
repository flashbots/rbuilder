//! Instantiation of run_backtest_build_block on our sample configuration.

use rbuilder::{
    backtest::build_block::synthetic_orders::run_backtest, live_builder::config::Config,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest::<Config>().await
}
