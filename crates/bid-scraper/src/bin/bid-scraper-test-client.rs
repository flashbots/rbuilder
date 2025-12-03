use alloy_primitives::utils::format_ether;
use bid_scraper::{
    bid_scraper_client::{run_nng_subscriber_with_retries, ScrapedBidsObs},
    types::ScrapedRelayBlockBid,
};
use rbuilder_config::LoggerConfig;
use std::{env, sync::Arc, time::Duration};
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;

struct ScrapedBidsPrinter {}
impl ScrapedBidsObs for ScrapedBidsPrinter {
    fn update_new_bid(&self, bid: ScrapedRelayBlockBid) {
        println!(
            "New bid {:?} ({:?}) Block {:?} val {:?}",
            bid.publisher_name,
            bid.publisher_type,
            bid.block_number,
            format_ether(bid.value)
        );
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("How do you expect me to know where to connect to get your bids? Should I guess? Try all ips and ports at random?\nHere is an idea: Pass me as a single parameter where to connect to.\nSomething like: {} tcp://127.0.01:5555",args[0]);
        return Ok(());
    }

    let logger_config = LoggerConfig {
        env_filter: "info".to_owned(),
        log_json: false,
        log_color: true,
    };
    logger_config.init_tracing()?;

    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            ctrl_c().await.unwrap_or_default();
            cancel.cancel()
        }
    });
    let publisher_url = args[1].clone();
    println!("Connecting to publishers..");
    let _ = tokio::spawn(run_nng_subscriber_with_retries(
        Arc::new(ScrapedBidsPrinter {}),
        cancel,
        publisher_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    ))
    .await;
    Ok(())
}
