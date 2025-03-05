use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::server::{Server};
use crate::primitives::kona::{SupervisorApiServer, ExecutingMessage, SafetyLevel};

struct SupervisorServer;

#[async_trait]
impl SupervisorApiServer for SupervisorServer {
    async fn check_messages(&self, messages: Vec<ExecutingMessage>, min_safety: SafetyLevel) -> RpcResult<()> {
        println!("Serving messages: {:?}", messages);
        for message in messages {
            println!("id {:?}", message.identifier.chainId);
        }
        Ok(())
    }
}
pub async fn start_mock_supervisor(port: u64) {
    let server = Server::builder().http_only().build(format!("127.0.0.1:{port}")).await.expect("build op-supervisor mock server");
    let handle = server.start(SupervisorServer.into_rpc());
    tokio::spawn(handle.stopped());
}