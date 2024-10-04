//! An implemention of the reth EthBundleApiServer trait.
//!
//! Should be refactored into standalone crate if required by other code.

use jsonrpsee::proc_macros::rpc;
use reth_rpc_types::mev::{CancelBundleRequest, EthBundleHash, EthSendBundle};
use transaction_pool_bundle_ext::BundlePoolOperations;

/// [`EthBundleApiServer`] implementation.
pub struct EthBundleMinimalApi<BundlePool> {
    pool: BundlePool,
}

impl<BundlePool> EthBundleMinimalApi<BundlePool> {
    pub fn new(pool: BundlePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl<BundlePool> EthCallBundleMinimalApiServer for EthBundleMinimalApi<BundlePool>
where
    BundlePool: BundlePoolOperations<Bundle = EthSendBundle> + 'static,
{
    async fn send_bundle(
        &self,
        bundle: EthSendBundle,
    ) -> jsonrpsee::core::RpcResult<EthBundleHash> {
        // TODO: Handle error
        self.pool.add_bundle(bundle).unwrap();

        // TODO: Hash and return
        todo!()
    }

    async fn cancel_bundle(&self, _request: CancelBundleRequest) -> jsonrpsee::core::RpcResult<()> {
        // TODO: Convert request.bundle_hash string into a hash.
        todo!()
        // self.pool.cancel_bundle(request.bundle_hash)
    }
}

/// A subset of the [EthBundleApi] API interface that only supports `eth_sendBundle` and
/// `eth_cancelBundle`.
#[rpc(server, namespace = "eth")]
pub trait EthCallBundleMinimalApi {
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: EthSendBundle)
        -> jsonrpsee::core::RpcResult<EthBundleHash>;

    #[method(name = "cancelBundle")]
    async fn cancel_bundle(&self, request: CancelBundleRequest) -> jsonrpsee::core::RpcResult<()>;
}
