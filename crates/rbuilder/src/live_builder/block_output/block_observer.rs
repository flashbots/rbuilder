//! Block observer interface for notifying external systems of built blocks.
//!
//! This module provides the `BlockObserver` trait that allows components to be
//! notified when new blocks are built. The primary use case is notifying the
//! EPBS Builder API server of new blocks so it can generate bids.

use crate::building::builders::Block;
use alloy_primitives::{BlockHash, B256};
use std::sync::Arc;

/// Observer that receives notifications when blocks are built.
///
/// Implementations can use these notifications to:
/// - Generate EPBS bids
pub trait BlockObserver: Send + Sync + std::fmt::Debug {
    /// Called when a new block has been finalized and is ready for submission.
    ///
    /// # Arguments
    /// * `slot` - The slot number this block is for
    /// * `parent_hash` - The execution layer parent block hash
    /// * `parent_block_root` - The beacon chain parent block root (from
    ///   `payload_attributes_event.data.parent_block_root`). Used as
    ///   `bid.parent_block_root` per the gloas spec.
    /// * `block` - The finalized block
    fn on_block_built(
        &self,
        slot: u64,
        parent_hash: BlockHash,
        parent_block_root: B256,
        block: &Block,
    );
}

/// A no-op observer that does nothing.
/// Used as a default when no observer is configured.
#[derive(Debug, Clone, Default)]
pub struct NoOpBlockObserver;

impl BlockObserver for NoOpBlockObserver {
    fn on_block_built(
        &self,
        _slot: u64,
        _parent_hash: BlockHash,
        _parent_block_root: B256,
        _block: &Block,
    ) {
        // No-op
    }
}

/// A multi observer that forwards notifications to multiple observers.
#[derive(Debug)]
pub struct MultiBlockObserver {
    observers: Vec<Arc<dyn BlockObserver>>,
}

impl MultiBlockObserver {
    pub fn new(observers: Vec<Arc<dyn BlockObserver>>) -> Self {
        Self { observers }
    }
}

impl BlockObserver for MultiBlockObserver {
    fn on_block_built(
        &self,
        slot: u64,
        parent_hash: BlockHash,
        parent_block_root: B256,
        block: &Block,
    ) {
        for observer in &self.observers {
            observer.on_block_built(slot, parent_hash, parent_block_root, block);
        }
    }
}
