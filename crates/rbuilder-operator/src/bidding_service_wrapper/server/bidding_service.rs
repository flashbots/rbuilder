//! Main entry point for the bidding service.
//! Just call run_bidding_service from main and it will create the server and run it.

use std::path::Path;

use crate::bidding_service_wrapper::{
    bidding_service_server::BiddingServiceServer,
    server::{
        bidding_service_server_initializer::BiddingServiceServerInitializer,
        bidding_service_with_reload::BiddingServiceFactory,
    },
    BidderVersionInfo,
};

use tokio::{
    net::UnixListener,
    select,
    signal::{ctrl_c, unix::SignalKind},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub async fn run_bidding_service(
    ipc_path: String,
    bidding_service_factory_factory: BiddingServiceFactory,
    version_info: Option<BidderVersionInfo>,
) -> eyre::Result<()> {
    info!("Bidding service starting");
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        select! {
            _ = ctrl_c() => {
            },
            _ = term() => {
            }
        }
        info!("Bidding service shutting down");
        cancel_clone.cancel()
    });
    tokio::spawn(async move {
        // Remove the socket file if it already exists
        if Path::new(&ipc_path).exists() {
            std::fs::remove_file(&ipc_path).unwrap();
        }
        let uds = UnixListener::bind(&ipc_path).unwrap();
        let server = BiddingServiceServerInitializer::new(
            Box::new(bidding_service_factory_factory),
            cancel.clone(),
            version_info,
        );
        tonic::transport::Server::builder()
            .add_service(BiddingServiceServer::new(server))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(uds),
                async {
                    cancel.cancelled().await;
                },
            )
            .await
            .unwrap();
    })
    .await?;
    Ok(())
}

/// Move this to some general place?
async fn term() -> std::io::Result<()> {
    tokio::signal::unix::signal(SignalKind::terminate())?
        .recv()
        .await;
    Ok(())
}
