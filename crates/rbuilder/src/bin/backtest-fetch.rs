//! Application to fetch orders from different sources (eg: mempool dumpster, external bundles db) and store them on a SQLite DB
//! to be used later (eg: backtest-build-block, backtest-build-range)

use rbuilder::{
    backtest::fetch::{
        backtest_fetch::run_backtest_fetch, data_source::DataSource,
        mempool::MempoolDumpsterDatasource,
    },
    live_builder::{cli::LiveBuilderConfig, config::Config},
};

async fn create_order_source(config: Config) -> eyre::Result<Box<dyn DataSource>> {
    // create paths for backtest_fetch_mempool_data_dir (i.e "~/.rbuilder/mempool-data" and ".../transactions")
    let backtest_fetch_mempool_data_dir = config.base_config().backtest_fetch_mempool_data_dir()?;
    let mempool_datasource = MempoolDumpsterDatasource::new(backtest_fetch_mempool_data_dir)?;
    Ok(Box::new(mempool_datasource))
}

#[tokio::main]
#[allow(clippy::needless_borrow)]
async fn main() -> eyre::Result<()> {
    run_backtest_fetch::<Config, _, _>(create_order_source).await
}
