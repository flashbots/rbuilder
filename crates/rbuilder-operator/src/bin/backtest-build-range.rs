use rbuilder::backtest::run_backtest_build_range;
use rbuilder_operator::flashbots_config::FlashbotsConfig;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest_build_range::<FlashbotsConfig>().await
}
