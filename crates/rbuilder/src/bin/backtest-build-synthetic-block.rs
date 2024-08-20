use rbuilder::{backtest::run_backtest_with_synthetic_bundles, live_builder::config::Config};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest_with_synthetic_bundles::<Config>().await
}
