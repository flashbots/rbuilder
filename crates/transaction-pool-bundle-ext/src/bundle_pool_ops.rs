//! Contains various implementations of [`BundlePoolOperations`] which can be
//! slotted into the node when
//!
//! Consider breaking into standalone crates if it grows too large.

use derive_more::From;
use reth_primitives::{Bytes, B256};
use reth_rpc_types::mev::EthSendBundle;

use crate::BundlePoolOperations;

/// [`BundlePoolOperations`] implementation which uses components of the
/// [`rbuilder`] under the hood to handle classic [`EthSendBundle`]s.
#[allow(unused)]
#[derive(Debug)]
pub struct RbuilderBundlePoolOps {
    order_pool: (),
    block_building_pool: (),
}

impl RbuilderBundlePoolOps {
    pub fn new() -> Result<Self, RbuilderBundlePoolOpsError> {
        // TODO: Init rbuilder OrderPool, OrderSimulationPool, BlockBuildingPool,
        // wire them up, and start them on an isolated thread.
        Ok(RbuilderBundlePoolOps {
            order_pool: (),
            block_building_pool: (),
        })
    }
}

impl BundlePoolOperations for RbuilderBundlePoolOps {
    type Bundle = EthSendBundle;
    type Error = RbuilderBundlePoolOpsError;
    /// Signed eth transaction
    type Transaction = Bytes;

    fn add_bundle(&self, _bundle: Self::Bundle) -> Result<(), Self::Error> {
        todo!()
    }

    fn cancel_bundle(&self, _hash: &B256) -> Result<(), Self::Error> {
        todo!()
    }

    fn get_transactions(&self) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error> {
        Ok(vec![])
    }
}

/// todo
#[derive(Debug, From)]
pub enum RbuilderBundlePoolOpsError {}
