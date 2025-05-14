//! Application to fetch orders from different sources (eg: mempool dumpster, external bundles db) and store them on a SQLite DB
//! to be used later (eg: backtest-build-block, backtest-build-range)

use rbuilder::{
    backtest::fetch::{
        backtest_fetch::run_backtest_fetch, data_source::DataSource, flashbots_db::RelayDB,
    },
    live_builder::{cli::LiveBuilderConfig, config::Config},
};

async fn create_bundle_source(config: Config) -> eyre::Result<Option<Box<dyn DataSource>>> {
    if let Some(db) = config.base_config().flashbots_db.clone() {
        let relay_db = RelayDB::from_url(db.value()?).await?;
        Ok(Some(Box::new(relay_db)))
    } else {
        Ok(None)
    }
}

#[tokio::main]
#[allow(clippy::needless_borrow)]
async fn main() -> eyre::Result<()> {
    run_backtest_fetch::<Config, _, _>(create_bundle_source).await
}
