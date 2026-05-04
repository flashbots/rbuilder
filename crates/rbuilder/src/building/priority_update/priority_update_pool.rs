use ahash::HashMap;
use parking_lot::RwLock;
use rbuilder_primitives::{
    proto::builder_priority_update_v1::{PriorityUpdate, PriorityUpdatePlacement},
    serialize::TxEncoding,
    Bundle, BundleReplacementData, BundleReplacementKey, Metadata, Order, PriorityUpdateClass,
    TxWithBlobsCreateError, LAST_BUNDLE_VERSION,
};
use rbuilder_utils::replace_event_scheduler::{
    ReplaceEventScheduler, ReplaceEventSchedulerSubscription,
};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

/// Ingress pool for priority updates received via the gRPC server.
///
/// Per-block schedulers store `Option<Arc<Order>>` keyed by `replacement_uuid`:
/// `Some` carries the decoded single-tx bundle, `None` is a cancellation
/// (empty `tx` field on the proto). Maintenance is driven by
/// [`Self::head_updated`].
#[derive(Debug, Default, Clone)]
pub struct PriorityUpdateIngressOrderpool {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    pools_for_block: HashMap<u64, ReplaceEventScheduler<Uuid, Option<Arc<Order>>>>,
    last_block: u64,
}

#[derive(Debug, Error)]
pub enum AddPriorityUpdateError {
    #[error("invalid replacement_uuid: {0}")]
    InvalidUuid(uuid::Error),
    #[error("update targets block {target} but head is already at {head}")]
    BlockTooOld { target: u64, head: u64 },
    #[error("failed to decode priority-update tx: {0}")]
    InvalidTx(TxWithBlobsCreateError),
}

impl PriorityUpdateIngressOrderpool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_priority_update(
        &self,
        update: PriorityUpdate,
    ) -> Result<(), AddPriorityUpdateError> {
        let uuid = Uuid::from_slice(&update.replacement_uuid)
            .map_err(AddPriorityUpdateError::InvalidUuid)?;
        let block_number = update.block_number;
        let seq = update.replacement_seq_number;

        let order = if update.tx.is_empty() {
            None
        } else {
            Some(priority_update_to_order(&update, uuid)?)
        };

        let scheduler = {
            let mut inner = self.inner.write();
            if block_number <= inner.last_block {
                return Err(AddPriorityUpdateError::BlockTooOld {
                    target: block_number,
                    head: inner.last_block,
                });
            }
            inner
                .pools_for_block
                .entry(block_number)
                .or_default()
                .clone()
        };

        scheduler.add_event(uuid, seq, order);
        Ok(())
    }

    pub fn subscribe(
        &self,
        block_number: u64,
    ) -> ReplaceEventSchedulerSubscription<Uuid, Option<Arc<Order>>> {
        let mut inner = self.inner.write();
        inner
            .pools_for_block
            .entry(block_number)
            .or_default()
            .subscribe()
    }

    pub fn head_updated(&self, new_block_number: u64) {
        let mut inner = self.inner.write();
        if new_block_number > inner.last_block {
            inner.last_block = new_block_number;
        }
        inner
            .pools_for_block
            .retain(|block, _| *block > new_block_number);
    }
}

fn priority_update_to_order(
    update: &PriorityUpdate,
    uuid: Uuid,
) -> Result<Arc<Order>, AddPriorityUpdateError> {
    let tx = TxEncoding::WithBlobData
        .decode(update.tx.clone().into())
        .map_err(AddPriorityUpdateError::InvalidTx)?;

    let class = match PriorityUpdatePlacement::try_from(update.placement) {
        Ok(PriorityUpdatePlacement::AlwaysTopOfBlock) => PriorityUpdateClass::ForceTopOfBlock,
        _ => PriorityUpdateClass::Regular,
    };

    let mut bundle = Bundle {
        version: LAST_BUNDLE_VERSION,
        block: Some(update.block_number),
        min_timestamp: None,
        max_timestamp: None,
        txs: vec![tx],
        reverting_tx_hashes: Vec::new(),
        dropping_tx_hashes: Vec::new(),
        hash: Default::default(),
        uuid: Default::default(),
        replacement_data: Some(BundleReplacementData {
            key: BundleReplacementKey::new(uuid, None),
            sequence_number: update.replacement_seq_number,
        }),
        signer: None,
        refund_identity: None,
        metadata: Metadata {
            priority_update_data: Some(class),
            ..Metadata::new_received_now()
        },
        refund: None,
        external_hash: None,
    };
    bundle.hash_slow();
    Ok(Arc::new(Order::Bundle(bundle)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip2718::Encodable2718;
    use rbuilder_primitives::TestDataGenerator;

    fn cancel(uuid: Uuid, block: u64, seq: u64) -> PriorityUpdate {
        PriorityUpdate {
            tx: Vec::new(),
            block_number: block,
            replacement_uuid: uuid.as_bytes().to_vec(),
            replacement_seq_number: seq,
            placement: PriorityUpdatePlacement::OptionalInclusion as i32,
            source: String::new(),
        }
    }

    fn update(gen: &mut TestDataGenerator, uuid: Uuid, block: u64, seq: u64) -> PriorityUpdate {
        let tx = gen.create_tx();
        let mut buf = Vec::new();
        tx.inner().encode_2718(&mut buf);
        PriorityUpdate {
            tx: buf,
            block_number: block,
            replacement_uuid: uuid.as_bytes().to_vec(),
            replacement_seq_number: seq,
            placement: PriorityUpdatePlacement::OptionalInclusion as i32,
            source: String::new(),
        }
    }

    fn drain(
        sub: &ReplaceEventSchedulerSubscription<Uuid, Option<Arc<Order>>>,
    ) -> Vec<(Uuid, Option<Arc<Order>>)> {
        let mut out = Vec::new();
        sub.pop_unprocessed_events(usize::MAX, &mut out);
        out
    }

    #[test]
    fn add_decodes_into_single_tx_bundle_with_replacement() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let mut gen = TestDataGenerator::default();
        let uuid = Uuid::new_v4();
        let sub = pool.subscribe(10).unwrap();
        pool.add_priority_update(update(&mut gen, uuid, 10, 1))
            .unwrap();

        let drained = drain(&sub);
        assert_eq!(drained.len(), 1);
        let (k, v) = &drained[0];
        assert_eq!(*k, uuid);
        let order = v.as_ref().unwrap();
        match order.as_ref() {
            Order::Bundle(b) => {
                assert_eq!(b.txs.len(), 1);
                assert_eq!(b.block, Some(10));
                let r = b.replacement_data.as_ref().unwrap();
                assert_eq!(r.key.id, uuid);
                assert_eq!(r.sequence_number, 1);
            }
            _ => panic!("expected bundle"),
        }
    }

    #[test]
    fn cancellation_is_stored_as_none() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let uuid = Uuid::new_v4();
        let sub = pool.subscribe(10).unwrap();
        pool.add_priority_update(cancel(uuid, 10, 1)).unwrap();
        let drained = drain(&sub);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, uuid);
        assert!(drained[0].1.is_none());
    }

    #[test]
    fn stale_seq_is_silently_dropped() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let uuid = Uuid::new_v4();
        let sub = pool.subscribe(10).unwrap();
        pool.add_priority_update(cancel(uuid, 10, 5)).unwrap();
        // same seq → dropped, but Ok
        pool.add_priority_update(cancel(uuid, 10, 5)).unwrap();
        // older seq → dropped
        pool.add_priority_update(cancel(uuid, 10, 4)).unwrap();
        let drained = drain(&sub);
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn rejects_block_at_or_below_head() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let uuid = Uuid::new_v4();
        pool.head_updated(10);
        assert!(matches!(
            pool.add_priority_update(cancel(uuid, 10, 1)),
            Err(AddPriorityUpdateError::BlockTooOld { .. })
        ));
        assert!(matches!(
            pool.add_priority_update(cancel(uuid, 9, 1)),
            Err(AddPriorityUpdateError::BlockTooOld { .. })
        ));
        pool.add_priority_update(cancel(uuid, 11, 1)).unwrap();
    }

    #[test]
    fn head_updated_drops_past_block_pools() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let uuid = Uuid::new_v4();
        pool.add_priority_update(cancel(uuid, 11, 1)).unwrap();
        pool.add_priority_update(cancel(uuid, 12, 1)).unwrap();
        pool.head_updated(11);
        assert!(pool.subscribe(11).is_none());
        assert!(pool.subscribe(12).is_some());
    }

    #[test]
    fn invalid_uuid_is_rejected() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let mut bad = cancel(Uuid::new_v4(), 10, 1);
        bad.replacement_uuid = vec![0u8; 4];
        assert!(matches!(
            pool.add_priority_update(bad),
            Err(AddPriorityUpdateError::InvalidUuid(_))
        ));
    }

    #[test]
    fn invalid_tx_is_rejected() {
        let pool = PriorityUpdateIngressOrderpool::new();
        let uuid = Uuid::new_v4();
        let mut bad = cancel(uuid, 10, 1);
        bad.tx = vec![0xff, 0x00];
        assert!(matches!(
            pool.add_priority_update(bad),
            Err(AddPriorityUpdateError::InvalidTx(_))
        ));
    }
}
