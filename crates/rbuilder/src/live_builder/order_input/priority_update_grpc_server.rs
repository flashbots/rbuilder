use rbuilder_primitives::proto::builder_priority_update_v1::{
    builder_priority_update_service_server::{
        BuilderPriorityUpdateService, BuilderPriorityUpdateServiceServer,
    },
    PriorityUpdate, PriorityUpdateResponse,
};
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{debug, error, info};

#[derive(Debug, Default)]
pub struct PriorityUpdateServiceStub;

#[tonic::async_trait]
impl BuilderPriorityUpdateService for PriorityUpdateServiceStub {
    async fn submit_priority_update(
        &self,
        request: Request<PriorityUpdate>,
    ) -> Result<Response<PriorityUpdateResponse>, Status> {
        let req = request.into_inner();
        debug!(?req, "received priority update");
        Ok(Response::new(PriorityUpdateResponse {
            error: String::new(),
        }))
    }
}

pub fn start_priority_update_grpc_server(
    addr: SocketAddr,
    global_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(%addr, "PriorityUpdateGrpcServer: starting");
        let result = Server::builder()
            .add_service(BuilderPriorityUpdateServiceServer::new(
                PriorityUpdateServiceStub,
            ))
            .serve_with_shutdown(addr, async {
                global_cancel.cancelled().await;
            })
            .await;
        if let Err(err) = result {
            error!(?err, "PriorityUpdateGrpcServer: terminated with error");
        } else {
            info!("PriorityUpdateGrpcServer: shut down");
        }
    })
}
