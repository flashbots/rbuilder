use crate::live_builder::order_input::replaceable_order_sink::ReplaceableOrderSink;
use rbuilder_primitives::{BundleReplacementData, Order, TransactionSignedEcRecoveredWithBlobs};
use tracing::trace;

/// Filters out [`Order`]s based on their blob type. [`Order`]s that do not
/// pass the filter are dropped not inserted. Preconfigured filters are
/// provided to remove pre-Fusaka [EIP-4844] blobs ([`Self::new_fusaka`]) or
/// post-Fusaka [EIP-7594] blobs ([`Self::new_pre_fusaka`]).
///
/// Since it's very unlikely that we have many wrong blobs we only filter on
/// [`ReplaceableOrderSink::insert_order`] without taking note of filtered
/// [`Order`]s.
///
/// [`ReplaceableOrderSink::remove_bundle`] calls are simply forwarded to the
/// sink, so it might try to remove a filtered [`Order`].
///
/// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
/// [EIP-7594]: https://eips.ethereum.org/EIPS/eip-7594
pub struct BlobTypeOrderFilter {
    sink: Box<dyn ReplaceableOrderSink>,

    /// Name of the filter for logging purposes.
    rule_name: &'static str,

    /// `true` if it likes the blob sidecar, `false` if it doesn't ([`Order`]
    /// gets filtered).
    filter_func: fn(&TransactionSignedEcRecoveredWithBlobs) -> bool,
}

impl std::fmt::Debug for BlobTypeOrderFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobTypeOrderFilter")
            .field("sink", &"<dyn ReplaceableOrderSink>")
            .field("rule", &self.rule_name)
            .finish()
    }
}

impl BlobTypeOrderFilter {
    /// Creates a new [`BlobTypeOrderFilter`], using the given `filter_func` to
    /// filter out orders.
    pub const fn new(
        sink: Box<dyn ReplaceableOrderSink>,
        rule_name: &'static str,
        filter_func: fn(&TransactionSignedEcRecoveredWithBlobs) -> bool,
    ) -> Self {
        Self {
            sink,
            rule_name,
            filter_func,
        }
    }

    /// Filters out [EIP-4844] style, allowing only [EIP-7594] style blobs.
    ///
    /// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
    /// [EIP-7594]: https://eips.ethereum.org/EIPS/eip-7594
    pub const fn new_fusaka(sink: Box<dyn ReplaceableOrderSink>) -> Self {
        fn fusaka(tx: &TransactionSignedEcRecoveredWithBlobs) -> bool {
            !tx.as_ref().is_eip4844() || tx.blobs_sidecar.is_eip7594()
        }

        Self::new(sink, "fusaka", fusaka)
    }

    /// Filters out [EIP-7594] style blobs, allowing only [EIP-4844] style.
    ///
    /// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
    /// [EIP-7594]: https://eips.ethereum.org/EIPS/eip-7594
    pub const fn new_pre_fusaka(sink: Box<dyn ReplaceableOrderSink>) -> Self {
        fn pre_fusaka(tx: &TransactionSignedEcRecoveredWithBlobs) -> bool {
            !tx.as_ref().is_eip4844() || tx.blobs_sidecar.is_eip4844()
        }

        Self::new(sink, "pre-fusaka", pre_fusaka)
    }
}

impl ReplaceableOrderSink for BlobTypeOrderFilter {
    fn insert_order(&mut self, order: Order) -> bool {
        if order
            .list_txs()
            .iter()
            .all(|(tx, _)| (self.filter_func)(tx))
        {
            self.sink.insert_order(order)
        } else {
            trace!(order_id = ?order.id(), rule = self.rule_name, "Order filtered out by BlobTypeOrderFilter");
            true
        }
    }

    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool {
        self.sink.remove_bundle(replacement_data)
    }

    fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }
}
