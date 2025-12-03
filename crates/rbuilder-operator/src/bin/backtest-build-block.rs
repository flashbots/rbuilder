use rbuilder::backtest::build_block::landed_block_from_db::run_backtest;
use rbuilder_operator::flashbots_config::FlashbotsConfig;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run_backtest::<FlashbotsConfig>().await
}
