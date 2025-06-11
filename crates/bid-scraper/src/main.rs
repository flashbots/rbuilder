use bid_scraper::bids_publisher::BidsPublisherService;
use bid_scraper::{Args, Service};
use clap::Parser;
use std::time::Duration;
use tokio::signal::ctrl_c;
use tokio::task::JoinHandle;
use tracing::info;

async fn supervisor() {
    loop {
        let handle = tokio::spawn(your_task());

        match handle.await {
            Ok(_) => {
                println!("Task completed normally");
                break; // Exit if task completes successfully
            }
            Err(e) if e.is_panic() => {
                println!("Task panicked: {:?}", e);
                println!("Restarting task...");
                // Optional: add delay before restart
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                println!("Task cancelled: {:?}", e);
                break; // Exit on cancellation
            }
        }
    }
}

async fn your_task() {
    println!("TASK HOLA!");
    tokio::time::sleep(Duration::from_secs(3)).await;
    panic!("CHAU");
}

#[tokio::main]
async fn main() {
    println!("Hello, world!");
    /*tokio::select! {
    _= supervisor()=>{},
    _=ctrl_c()=>{}
    }*/
    tracing_subscriber::fmt::init();

    // when one task crashes we crash the whole program
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    let args = Args::parse();

    info!("Initializing service...");
    let service = BidsPublisherService::new(args.clone()).await;
    info!("Service initialized!");

    service.run().await;

    println!("Bye!!!!");
}
