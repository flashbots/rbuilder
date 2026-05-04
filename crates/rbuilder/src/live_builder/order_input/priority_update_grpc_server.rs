use crate::building::priority_update::priority_update_pool::PriorityUpdateIngressOrderpool;
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
use tracing::{error, info, trace};

#[derive(Debug)]
pub struct PriorityUpdateGrpcService {
    pool: PriorityUpdateIngressOrderpool,
}

impl PriorityUpdateGrpcService {
    pub fn new(pool: PriorityUpdateIngressOrderpool) -> Self {
        Self { pool }
    }
}

#[tonic::async_trait]
impl BuilderPriorityUpdateService for PriorityUpdateGrpcService {
    async fn submit_priority_update(
        &self,
        request: Request<PriorityUpdate>,
    ) -> Result<Response<PriorityUpdateResponse>, Status> {
        let req = request.into_inner();
        trace!(
            block_number = req.block_number,
            seq = req.replacement_seq_number,
            source = %req.source,
            "received priority update"
        );
        let response = match self.pool.add_priority_update(req) {
            Ok(()) => PriorityUpdateResponse {
                error: String::new(),
            },
            Err(err) => {
                trace!(?err, "priority update rejected");
                PriorityUpdateResponse {
                    error: err.to_string(),
                }
            }
        };
        Ok(Response::new(response))
    }
}

pub fn start_priority_update_grpc_server(
    addr: SocketAddr,
    pool: PriorityUpdateIngressOrderpool,
    global_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(%addr, "PriorityUpdateGrpcServer: starting");
        let result = Server::builder()
            .add_service(BuilderPriorityUpdateServiceServer::new(
                PriorityUpdateGrpcService::new(pool),
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
