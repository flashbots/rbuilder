use rbuilder::backtest::redistribute::run_backtest_redistribute;
use rbuilder::live_builder::config::Config;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest_redistribute::<Config>().await
}
