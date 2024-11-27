use rbuilder::{backtest::redistribute::run_backtest_redistribute, live_builder::config::Config};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest_redistribute::<Config>().await
}
