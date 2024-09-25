//! An implemention of the reth EthBundleApiServer trait.

use reth_primitives::{Bytes, B256};
use reth_rpc_api::EthBundleApiServer;
use reth_rpc_types::mev::{
    CancelBundleRequest, CancelPrivateTransactionRequest, EthBundleHash, EthCallBundle,
    EthCallBundleResponse, EthSendBundle, PrivateTransactionRequest,
};

/// [`EthBundleApiServer`] implementation.
pub struct EthBundleApi;

#[async_trait::async_trait]
impl EthBundleApiServer for EthBundleApi {
    async fn send_bundle(
        &self,
        _bundle: EthSendBundle,
    ) -> jsonrpsee::core::RpcResult<EthBundleHash> {
        unimplemented!()
    }

    async fn call_bundle(
        &self,
        _request: EthCallBundle,
    ) -> jsonrpsee::core::RpcResult<EthCallBundleResponse> {
        unimplemented!()
    }

    async fn cancel_bundle(&self, _request: CancelBundleRequest) -> jsonrpsee::core::RpcResult<()> {
        unimplemented!()
    }

    async fn send_private_transaction(
        &self,
        _request: PrivateTransactionRequest,
    ) -> jsonrpsee::core::RpcResult<B256> {
        unimplemented!()
    }

    async fn send_private_raw_transaction(
        &self,
        _bytes: Bytes,
    ) -> jsonrpsee::core::RpcResult<B256> {
        unimplemented!()
    }

    async fn cancel_private_transaction(
        &self,
        _request: CancelPrivateTransactionRequest,
    ) -> jsonrpsee::core::RpcResult<bool> {
        unimplemented!()
    }
}
