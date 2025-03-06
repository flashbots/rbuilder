use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::server::{Server, ServerHandle};
use crate::primitives::kona::{SupervisorApiServer, ExecutingMessage, SafetyLevel};
use alloy_sol_types::sol;

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
pub async fn start_mock_supervisor(port: u64) -> ServerHandle {
    let server = Server::builder().http_only().build(format!("127.0.0.1:{port}")).await.expect("build op-supervisor mock server");
    server.start(SupervisorServer.into_rpc())
}

sol! {
    /// @notice The struct for a pointer to a message payload in a remote (or local) chain.
    #[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct MessageIdentifier {
        address origin;
        uint256 blockNumber;
        uint256 logIndex;
        uint256 timestamp;
        #[serde(rename = "chainID")]
        uint256 chainId;
    }

    /// @notice Relays a cross chain message to the destination chain.
    /// @param _id      Identifier of the message.
    /// @param _sentMessage Message payload to call target with.
    function relayMessage(
        MessageIdentifier calldata _id,
        bytes calldata _sentMessage
    ) external;
}