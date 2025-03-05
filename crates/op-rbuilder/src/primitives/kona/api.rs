//! [Source](https://github.com/op-rs/kona/blob/a1d8ea603960cb4bd3cc19784f7c3365352f1849/crates/node/rpc/src/api.rs)
use crate::primitives::kona::{ExecutingMessage, SafetyLevel};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// Supervisor API for interop.
#[rpc(server, client, namespace = "supervisor")]
pub trait SupervisorApi {
    /// Checks if the given messages meet the given minimum safety level.
    #[method(name = "checkMessages")]
    async fn check_messages(
        &self,
        messages: Vec<ExecutingMessage>,
        min_safety: SafetyLevel,
    ) -> RpcResult<()>;
}
