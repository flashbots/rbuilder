//! Order types used as elements for block building.

pub mod fmt;
pub mod mev_boost;
pub mod order_builder;
pub mod serialize;
mod test_data_generator;

use crate::building::evm_inspector::UsedStateTrace;
use alloy_primitives::{Bytes, TxHash};
use derivative::Derivative;
use ethereum_consensus::deneb::polynomial_commitments::BYTES_PER_COMMITMENT;
use integer_encoding::VarInt;
use reth::primitives::{
    keccak256,
    kzg::{Blob, Bytes48, BYTES_PER_BLOB, BYTES_PER_PROOF},
    Address, BlobTransactionSidecar, PooledTransactionsElement, TransactionSigned,
    TransactionSignedEcRecovered, B256,
};
use revm::primitives::U256;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, collections::HashMap, fmt::Display, str::FromStr, sync::Arc};
pub use test_data_generator::TestDataGenerator;
use thiserror::Error;
use uuid::Uuid;

/// Extra metadata for ShareBundle/Bundle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Metadata {
    pub received_at_timestamp: time::OffsetDateTime,
}

impl Metadata {
    pub fn with_current_received_at() -> Self {
        Self {
            received_at_timestamp: time::OffsetDateTime::now_utc(),
        }
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self::with_current_received_at()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct AccountNonce {
    pub nonce: u64,
    pub account: Address,
}
impl AccountNonce {
    pub fn with_nonce(self, nonce: u64) -> Self {
        AccountNonce {
            account: self.account,
            nonce,
        }
    }
}

/// BundledTxInfo should replace Nonce in the future.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundledTxInfo {
    pub nonce: AccountNonce,
    /// optional -> can revert and the bundle continues.
    pub optional: bool,
}

/// @Pending: Delete and replace all uses by BundledTxInfo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Nonce {
    pub nonce: u64,
    pub address: Address,
    pub optional: bool,
}

/// Information regarding a new/update replaceable Bundle/ShareBundle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplacementData<KeyType> {
    pub key: KeyType,
    /// Due to simulation async problems Bundle updates can arrive out of order.
    /// sequence_number allows us to keep always the last one.
    pub sequence_number: u64,
}

impl<KeyType: Clone> ReplacementData<KeyType> {
    /// Next sequence_number, useful for testing.
    pub fn next(&self) -> Self {
        Self {
            key: self.key.clone(),
            sequence_number: self.sequence_number + 1,
        }
    }
}

pub type BundleReplacementData = ReplacementData<BundleReplacementKey>;

/// Bundle sent to us usually by a searcher via eth_sendBundle (https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint#eth_sendbundle).
#[derive(Derivative)]
#[derivative(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Bundle {
    pub block: u64,
    pub min_timestamp: Option<u64>,
    pub max_timestamp: Option<u64>,
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    pub reverting_tx_hashes: Vec<B256>,
    /// Virtual hash generated by concatenating all txs hashes (+some more info) and hashing them.
    /// See [Bundle::hash_slow] for more details.
    pub hash: B256,
    /// Unique id we generate.
    pub uuid: Uuid,
    /// Unique id, bundle signer.
    /// The unique id was generated by the sender and is used for updates/cancellations.
    /// Bundle signer is redundant with self.signer.
    pub replacement_data: Option<BundleReplacementData>,
    pub signer: Option<Address>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,
}

impl Bundle {
    pub fn can_execute_with_block_base_fee(&self, block_base_fee: u128) -> bool {
        can_execute_with_block_base_fee(self.list_txs(), block_base_fee)
    }

    /// BundledTxInfo for all the child txs.
    pub fn nonces(&self) -> Vec<Nonce> {
        let txs = self
            .txs
            .iter()
            .map(|tx| (tx, self.reverting_tx_hashes.contains(&tx.tx.hash)));
        bundle_nonces(txs)
    }

    fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.txs
            .iter()
            .map(|tx| (tx, self.reverting_tx_hashes.contains(&tx.tx.hash())))
            .collect()
    }

    /// Recalculate bundle hash and uuid.
    /// Hash is computed from child tx hashes + reverting_tx_hashes.
    pub fn hash_slow(&mut self) {
        let hash = self
            .txs
            .iter()
            .flat_map(|tx| tx.hash().0.to_vec())
            .collect::<Vec<_>>();
        self.hash = keccak256(hash);

        let uuid = {
            // Block, hash, reverting hashes.
            let mut buff = Vec::with_capacity(8 + 32 + 32 * self.reverting_tx_hashes.len());
            {
                let block = self.block as i64;
                buff.append(&mut block.encode_var_vec());
            }
            buff.extend_from_slice(self.hash.as_slice());
            self.reverting_tx_hashes.sort();
            for reverted_hash in &self.reverting_tx_hashes {
                buff.extend_from_slice(reverted_hash.as_slice());
            }
            let hash = {
                let mut res = [0u8; 16];
                let mut hasher = Sha256::new();
                // We write 16 zeroes to replicate golang hashing behavior.
                hasher.update(res);
                hasher.update(&buff);
                let output = hasher.finalize();
                res.copy_from_slice(&output.as_slice()[0..16]);
                res
            };
            uuid::Builder::from_sha1_bytes(hash).into_uuid()
        };
        self.uuid = uuid;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TxRevertBehavior {
    /// Tx in a bundle can't revert.
    NotAllowed,
    /// If the tx reverts it will be included. This is the old "can_revert" boolean.
    AllowedIncluded,
    /// If the tx reverts we will ignore it.
    AllowedExcluded,
}

impl TxRevertBehavior {
    /// Backwards compatibility.
    pub fn from_old_bool(can_revert: bool) -> Self {
        if can_revert {
            TxRevertBehavior::AllowedIncluded
        } else {
            TxRevertBehavior::NotAllowed
        }
    }
    pub fn can_revert(&self) -> bool {
        match self {
            TxRevertBehavior::NotAllowed => false,
            TxRevertBehavior::AllowedIncluded | TxRevertBehavior::AllowedExcluded => true,
        }
    }
}

/// Tx as part of a mev share body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareBundleTx {
    pub tx: TransactionSignedEcRecoveredWithBlobs,
    pub revert_behavior: TxRevertBehavior,
}

impl ShareBundleTx {
    pub fn hash(&self) -> TxHash {
        self.tx.hash()
    }
}

/// Body element of a mev share bundle.
/// [`ShareBundleInner::body`] is formed by several of these.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ShareBundleBody {
    Tx(ShareBundleTx),
    Bundle(ShareBundleInner),
}

impl ShareBundleBody {
    pub fn refund_config(&self) -> Option<Vec<RefundConfig>> {
        match self {
            Self::Tx(sbundle_tx) => Some(vec![RefundConfig {
                address: sbundle_tx.tx.signer(),
                percent: 100,
            }]),
            Self::Bundle(b) => b.refund_config(),
        }
    }
}

/// Mev share contains 2 types of txs:
/// - User txs: simple txs sent to us to be protected and to give kickbacks to the user.
/// - Searcher txs: Txs added by a searcher to extract MEV from the user txs.
/// Refund points to the user txs on the body and has the kickback percentage for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refund {
    /// Index of the ShareBundleInner::body for which this applies.
    pub body_idx: usize,
    /// Percent of the profit going back to the user as kickback.
    pub percent: usize,
}

/// Users can specify how to get kickbacks and this is propagated by the MEV-Share Node to us.
/// We get this configuration as multiple RefundConfigs, then the refunds are payed to the specified addresses in the indicated percentages.
/// The sum of all RefundConfig::percent on a mev share bundle should be 100%.
/// See [ShareBundleInner::refund_config] for more details.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefundConfig {
    pub address: Address,
    pub percent: usize,
}

/// sub bundle as part of a mev share body
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareBundleInner {
    pub body: Vec<ShareBundleBody>,
    pub refund: Vec<Refund>,
    /// Optional RefundConfig for this ShareBundleInner. see [ShareBundleInner::refund_config] for more details.
    pub refund_config: Vec<RefundConfig>,
    /// We are allowed to skip this sub bundle (either because of inner reverts or any other reason).
    /// Added specifically to allow same user sbundle merging since we stick together many sbundles and allow some of them to fail.
    pub can_skip: bool,
    /// Patch to track the original orders when performing order merging (see [`ShareBundleMerger`]).
    pub original_order_id: Option<OrderId>,
}

impl ShareBundleInner {
    fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.body
            .iter()
            .flat_map(|b| match b {
                ShareBundleBody::Tx(sbundle_tx) => {
                    vec![(&sbundle_tx.tx, sbundle_tx.revert_behavior.can_revert())]
                }
                ShareBundleBody::Bundle(bundle) => bundle.list_txs(),
            })
            .collect()
    }

    /// Refunds config for the ShareBundleInner.
    /// refund_config not empty -> we use it
    /// refund_config empty:
    ///     - body empty (illegal?) -> None
    ///     - body not empty -> first child refund_config()
    /// Since for ShareBundleBody::Tx we use 100% to the signer of the tx (see [ShareBundleBody::refund_config]) as RefundConfig this basically
    /// makes DFS looking for the first ShareBundleInner with explicit RefundConfig or the first Tx.
    pub fn refund_config(&self) -> Option<Vec<RefundConfig>> {
        if !self.refund_config.is_empty() {
            return Some(self.refund_config.clone());
        }
        if self.body.is_empty() {
            return None;
        }
        self.body[0].refund_config()
    }

    // Recalculate bundle hash.
    pub fn hash_slow(&self) -> B256 {
        let hashes = self
            .body
            .iter()
            .map(|b| match b {
                ShareBundleBody::Tx(sbundle_tx) => sbundle_tx.tx.hash(),
                ShareBundleBody::Bundle(inner) => inner.hash_slow(),
            })
            .collect::<Vec<_>>();
        if hashes.len() == 1 {
            hashes[0]
        } else {
            keccak256(
                hashes
                    .into_iter()
                    .flat_map(|h| h.0.to_vec())
                    .collect::<Vec<_>>(),
            )
        }
    }
}

/// Uniquely identifies a replaceable sbundle or bundle
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct ReplacementKey {
    pub id: Uuid,
    pub signer: Address,
}

pub type ShareBundleReplacementData = ReplacementData<ShareBundleReplacementKey>;

/// Preprocessed Share bundle originated by mev_sendBundle (https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint#eth_sendbundle)
/// Instead of having hashes (as in the original definition) it contains the actual txs.
#[derive(Derivative)]
#[derivative(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareBundle {
    /// Hash for the ShareBundle (also used in OrderId::ShareBundle).
    /// See [ShareBundle::hash_slow] for more details.
    pub hash: B256,
    pub block: u64,
    pub max_block: u64,
    pub inner_bundle: ShareBundleInner,
    pub signer: Option<Address>,
    /// data that uniquely identifies this ShareBundle for update or cancellation
    pub replacement_data: Option<ShareBundleReplacementData>,
    /// Only used internally when we build a virtual (not part of the orderflow) ShareBundle from other orders.
    pub original_orders: Vec<Order>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,
}

impl ShareBundle {
    pub fn can_execute_with_block_base_fee(&self, block_base_fee: u128) -> bool {
        can_execute_with_block_base_fee(self.list_txs(), block_base_fee)
    }

    pub fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.inner_bundle.list_txs()
    }

    /// BundledTxInfo for all the child txs
    pub fn nonces(&self) -> Vec<Nonce> {
        bundle_nonces(self.inner_bundle.list_txs().into_iter())
    }

    pub fn flatten_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.inner_bundle.list_txs()
    }

    // Recalculate bundle hash.
    /// Sadly it's not perfect since it only hashes inner txs in DFS, so tree structure and all other cfg is lost.
    pub fn hash_slow(&mut self) {
        self.hash = self.inner_bundle.hash_slow();
    }

    /// Patch to store the original orders for a merged order (see [`ShareBundleMerger`])
    pub fn original_orders(&self) -> Vec<&Order> {
        self.original_orders.iter().collect()
    }

    /// see [`ShareBundleMerger`]
    pub fn is_merged_order(&self) -> bool {
        !self.original_orders.is_empty()
    }
}

/// First idea to handle blobs might change.
/// Don't like the fact that blobs_sidecar exists no matter if TransactionSignedEcRecovered contains a non blob tx.
#[derive(Derivative)]
#[derivative(Debug, Clone, PartialEq, Eq)]
pub struct TransactionSignedEcRecoveredWithBlobs {
    pub tx: TransactionSignedEcRecovered,
    /// Will have a non empty BlobTransactionSidecar if TransactionSignedEcRecovered is 4844
    pub blobs_sidecar: Arc<BlobTransactionSidecar>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,
}

impl AsRef<TransactionSignedEcRecovered> for TransactionSignedEcRecoveredWithBlobs {
    fn as_ref(&self) -> &TransactionSignedEcRecovered {
        &self.tx
    }
}

impl AsRef<TransactionSigned> for TransactionSignedEcRecoveredWithBlobs {
    fn as_ref(&self) -> &TransactionSigned {
        &self.tx
    }
}

#[derive(Error, Debug)]
pub enum RawTxWithBlobsConvertError {
    #[error("Failed to decode transaction, error: {0}")]
    FailedToDecodeTransaction(alloy_rlp::Error),
    #[error("Invalid transaction signature")]
    InvalidTransactionSignature,
    #[error("Invalid transaction signature")]
    UnexpectedError,
}

impl TransactionSignedEcRecoveredWithBlobs {
    /// Creates a Self with empty blobs sidecar ONLY if the tx has no blobs.
    pub fn new_no_blobs(tx: TransactionSignedEcRecovered) -> Option<Self> {
        if tx.transaction.blob_versioned_hashes().is_some() {
            return None;
        }
        Some(Self {
            tx,
            blobs_sidecar: Arc::new(BlobTransactionSidecar::default()),
            metadata: Default::default(),
        })
    }

    pub fn hash(&self) -> TxHash {
        self.tx.hash()
    }

    pub fn signer(&self) -> Address {
        self.tx.signer()
    }

    /// Encodes the "raw" canonical format of transaction (NOT the one used in `eth_sendRawTransaction`) BLOB DATA IS NOT ENCODED.
    /// I intensionally omitted the version with blob data since we don't use it and may lead to confusions/bugs.
    pub fn envelope_encoded_no_blobs(&self) -> Bytes {
        self.tx.envelope_encoded()
    }

    /// Decodes the "raw" format of transaction (e.g. `eth_sendRawTransaction`) with the blob data (network format)
    pub fn decode_enveloped_with_real_blobs(
        raw_tx: Bytes,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, RawTxWithBlobsConvertError> {
        let raw_tx = &mut raw_tx.as_ref();
        let pooled_tx: PooledTransactionsElement =
            PooledTransactionsElement::decode_enveloped(raw_tx)
                .map_err(RawTxWithBlobsConvertError::FailedToDecodeTransaction)?;
        let signer = pooled_tx
            .recover_signer()
            .ok_or(RawTxWithBlobsConvertError::InvalidTransactionSignature)?;
        match pooled_tx {
            PooledTransactionsElement::Legacy {
                transaction: _,
                signature: _,
                hash: _,
            } => TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                pooled_tx.into_ecrecovered_transaction(signer),
            )
            .ok_or(RawTxWithBlobsConvertError::UnexpectedError),
            PooledTransactionsElement::Eip2930 {
                transaction: _,
                signature: _,
                hash: _,
            } => TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                pooled_tx.into_ecrecovered_transaction(signer),
            )
            .ok_or(RawTxWithBlobsConvertError::UnexpectedError),
            PooledTransactionsElement::Eip1559 {
                transaction: _,
                signature: _,
                hash: _,
            } => TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                pooled_tx.into_ecrecovered_transaction(signer),
            )
            .ok_or(RawTxWithBlobsConvertError::UnexpectedError),
            PooledTransactionsElement::BlobTransaction(blob_tx) => {
                let (tx, sidecar) = blob_tx.into_parts();
                Ok(TransactionSignedEcRecoveredWithBlobs {
                    tx: tx.with_signer(signer),
                    blobs_sidecar: Arc::new(sidecar),
                    metadata: Metadata::default(),
                })
            }
        }
    }
    /// Decodes the "raw" canonical format of transaction (NOT the one used in `eth_sendRawTransaction`) generating fake blob data for backtesting
    pub fn decode_enveloped_with_fake_blobs(
        raw_tx: Bytes,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, RawTxWithBlobsConvertError> {
        let decoded = TransactionSigned::decode_enveloped(&mut raw_tx.as_ref())
            .map_err(RawTxWithBlobsConvertError::FailedToDecodeTransaction)?;
        let tx = decoded
            .into_ecrecovered()
            .ok_or(RawTxWithBlobsConvertError::InvalidTransactionSignature)?;
        let mut fake_sidecar = BlobTransactionSidecar::default();
        for _ in 0..tx.blob_versioned_hashes().map_or(0, |hashes| hashes.len()) {
            fake_sidecar.blobs.push(Blob::from([0u8; BYTES_PER_BLOB]));
            fake_sidecar
                .commitments
                .push(Bytes48::from([0u8; BYTES_PER_COMMITMENT]));
            fake_sidecar
                .proofs
                .push(Bytes48::from([0u8; BYTES_PER_PROOF]));
        }
        Ok(TransactionSignedEcRecoveredWithBlobs {
            tx,
            blobs_sidecar: Arc::new(fake_sidecar),
            metadata: Metadata::default(),
        })
    }
}

impl std::hash::Hash for TransactionSignedEcRecoveredWithBlobs {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        //This is enogth to identify the tx
        self.tx.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MempoolTx {
    pub tx_with_blobs: TransactionSignedEcRecoveredWithBlobs,
}

impl MempoolTx {
    pub fn new(tx_with_blobs: TransactionSignedEcRecoveredWithBlobs) -> Self {
        Self { tx_with_blobs }
    }
}

/// Main type used for block building, we build blocks as sequences of Orders
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Order {
    Bundle(Bundle),
    Tx(MempoolTx),
    ShareBundle(ShareBundle),
}

/// Uniquely identifies a replaceable sbundle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShareBundleReplacementKey(ReplacementKey);
impl ShareBundleReplacementKey {
    pub fn new(id: Uuid, signer: Address) -> Self {
        Self(ReplacementKey { id, signer })
    }

    pub fn key(&self) -> ReplacementKey {
        self.0
    }
}

/// Uniquely identifies a replaceable bundle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BundleReplacementKey(ReplacementKey);
impl BundleReplacementKey {
    pub fn new(id: Uuid, signer: Address) -> Self {
        Self(ReplacementKey { id, signer })
    }
    pub fn key(&self) -> ReplacementKey {
        self.0
    }
}

/// General type for both BundleReplacementKey and ShareBundleReplacementKey
/// Even although BundleReplacementKey and ShareBundleReplacementKey have the same info they are kept
/// as different types to avoid bugs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OrderReplacementKey {
    Bundle(BundleReplacementKey),
    ShareBundle(ShareBundleReplacementKey),
}

impl Order {
    /// Partial execution is valid as long as some tx is left.
    pub fn can_execute_with_block_base_fee(&self, block_base_fee: u128) -> bool {
        match self {
            Order::Bundle(bundle) => bundle.can_execute_with_block_base_fee(block_base_fee),
            Order::Tx(tx) => tx.tx_with_blobs.tx.max_fee_per_gas() >= block_base_fee,
            Order::ShareBundle(bundle) => bundle.can_execute_with_block_base_fee(block_base_fee),
        }
    }

    /// Patch to allow virtual orders not originated from a source.
    /// This patch allows to easily implement sbundle merging see ([`ShareBundleMerger`]) and keep the original
    /// orders for post execution work (eg: logs).
    /// Non virtual orders should return self
    pub fn original_orders(&self) -> Vec<&Order> {
        match self {
            Order::Bundle(_) => vec![self],
            Order::Tx(_) => vec![self],
            Order::ShareBundle(sb) => {
                let res = sb.original_orders();
                if res.is_empty() {
                    //fallback to this order
                    vec![self]
                } else {
                    res
                }
            }
        }
    }

    /// BundledTxInfo for all the child txs
    pub fn nonces(&self) -> Vec<Nonce> {
        match self {
            Order::Bundle(bundle) => bundle.nonces(),
            Order::Tx(tx) => vec![Nonce {
                nonce: tx.tx_with_blobs.tx.nonce(),
                address: tx.tx_with_blobs.tx.signer(),
                optional: false,
            }],
            Order::ShareBundle(bundle) => bundle.nonces(),
        }
    }

    pub fn id(&self) -> OrderId {
        match self {
            Order::Bundle(bundle) => OrderId::Bundle(bundle.uuid),
            Order::Tx(tx) => OrderId::Tx(tx.tx_with_blobs.tx.hash()),
            Order::ShareBundle(bundle) => OrderId::ShareBundle(bundle.hash),
        }
    }

    pub fn is_tx(&self) -> bool {
        matches!(self, Order::Tx(_))
    }

    /// Vec<(Tx, allowed to revert)>
    pub fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        match self {
            Order::Bundle(bundle) => bundle.list_txs(),
            Order::Tx(tx) => vec![(&tx.tx_with_blobs, true)],
            Order::ShareBundle(bundle) => bundle.list_txs(),
        }
    }

    pub fn replacement_key(&self) -> Option<OrderReplacementKey> {
        self.replacement_key_and_sequence_number()
            .map(|(key, _)| key)
    }

    pub fn replacement_key_and_sequence_number(&self) -> Option<(OrderReplacementKey, u64)> {
        match self {
            Order::Bundle(bundle) => bundle.replacement_data.as_ref().map(|r| {
                (
                    OrderReplacementKey::Bundle(r.clone().key),
                    r.sequence_number,
                )
            }),
            Order::Tx(_) => None,
            Order::ShareBundle(sbundle) => sbundle.replacement_data.as_ref().map(|r| {
                (
                    OrderReplacementKey::ShareBundle(r.clone().key),
                    r.sequence_number,
                )
            }),
        }
    }

    pub fn has_blobs(&self) -> bool {
        self.list_txs()
            .iter()
            .any(|(tx, _)| !tx.blobs_sidecar.blobs.is_empty())
    }

    pub fn target_block(&self) -> Option<u64> {
        match self {
            Order::Bundle(bundle) => Some(bundle.block),
            Order::Tx(_) => None,
            Order::ShareBundle(bundle) => Some(bundle.block),
        }
    }

    /// Address that signed the bundle request
    pub fn signer(&self) -> Option<Address> {
        match self {
            Order::Bundle(bundle) => bundle.signer,
            Order::ShareBundle(bundle) => bundle.signer,
            Order::Tx(_) => None,
        }
    }

    pub fn metadata(&self) -> &Metadata {
        match self {
            Order::Bundle(bundle) => &bundle.metadata,
            Order::Tx(tx) => &tx.tx_with_blobs.metadata,
            Order::ShareBundle(bundle) => &bundle.metadata,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SimValue {
    /// profit as coinbase delta after executing an Order
    pub coinbase_profit: U256,
    pub gas_used: u64,
    #[serde(default)]
    pub blob_gas_used: u64,
    /// This is computed as coinbase_profit/gas_used so it includes not only gas tip but also payments made directly to coinbase
    pub mev_gas_price: U256,
    /// Kickbacks paid during simulation as (receiver, amount)
    pub paid_kickbacks: Vec<(Address, U256)>,
}

impl SimValue {
    pub fn new(
        coinbase_profit: U256,
        gas_used: u64,
        blob_gas_used: u64,
        paid_kickbacks: Vec<(Address, U256)>,
    ) -> Self {
        let mev_gas_price = if gas_used != 0 {
            coinbase_profit / U256::from(gas_used)
        } else {
            U256::ZERO
        };
        Self {
            coinbase_profit,
            gas_used,
            blob_gas_used,
            mev_gas_price,
            paid_kickbacks,
        }
    }
}

/// Order simulated (usually on top of block) + SimValue
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedOrder {
    pub order: Order,
    pub sim_value: SimValue,
    /// Not used, we should kill this
    pub prev_order: Option<OrderId>,
    /// Info about read/write slots during the simulation to help figure out what the Order is doing.
    pub used_state_trace: Option<UsedStateTrace>,
}

impl SimulatedOrder {
    pub fn id(&self) -> OrderId {
        self.order.id()
    }

    pub fn nonces(&self) -> Vec<Nonce> {
        self.order.nonces()
    }
}

/// Unique OrderId used along the whole builder.
/// Sadly it's not perfect since we still might have some collisions (eg: ShareBundle is the tx tree hash which does not include all the other cfg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderId {
    Tx(B256),
    Bundle(Uuid),
    ShareBundle(B256),
}

impl OrderId {
    pub fn fixed_bytes(&self) -> B256 {
        match self {
            Self::Tx(hash) | Self::ShareBundle(hash) => *hash,
            Self::Bundle(uuid) => {
                let mut out = [0u8; 32];
                out[0..16].copy_from_slice(uuid.as_bytes());
                B256::new(out)
            }
        }
    }
}

impl FromStr for OrderId {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(hash_str) = s.strip_prefix("tx:") {
            let hash = B256::from_str(hash_str)?;
            Ok(Self::Tx(hash))
        } else if let Some(id_str) = s.strip_prefix("bundle:") {
            let uuid = Uuid::from_str(id_str)?;
            Ok(Self::Bundle(uuid))
        } else if let Some(hash_str) = s.strip_prefix("sbundle:") {
            let hash = B256::from_str(hash_str)?;
            Ok(Self::ShareBundle(hash))
        } else {
            Err(eyre::eyre!("invalid order id"))
        }
    }
}

/// DON'T CHANGE this since this implements ToString which is used for serialization (deserialization on FromStr above)
impl Display for OrderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tx(hash) => write!(f, "tx:{:?}", hash),
            Self::Bundle(uuid) => write!(f, "bundle:{:?}", uuid),
            Self::ShareBundle(hash) => write!(f, "sbundle:{:?}", hash),
        }
    }
}

impl PartialOrd for OrderId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderId {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(id: &OrderId) -> usize {
            match id {
                OrderId::Tx(_) => 1,
                OrderId::Bundle(_) => 2,
                OrderId::ShareBundle(_) => 3,
            }
        }

        self.fixed_bytes()
            .cmp(&other.fixed_bytes())
            .then_with(|| rank(self).cmp(&rank(other)))
    }
}

fn bundle_nonces<'a>(
    txs: impl Iterator<Item = (&'a TransactionSignedEcRecoveredWithBlobs, bool)>,
) -> Vec<Nonce> {
    let mut nonces: HashMap<Address, Nonce> = HashMap::new();
    for (tx, optional) in txs.map(|(tx_with_blob, optional)| (&tx_with_blob.tx, optional)) {
        nonces
            .entry(tx.signer())
            .and_modify(|nonce| {
                if nonce.nonce > tx.nonce() {
                    nonce.nonce = tx.nonce();
                    nonce.optional = optional;
                }
            })
            .or_insert(Nonce {
                nonce: tx.nonce(),
                address: tx.signer(),
                optional,
            });
    }
    let mut res = nonces.into_values().collect::<Vec<_>>();
    res.sort_by_key(|nonce| nonce.address);
    res
}

/// Checks that at least one tx can execute and that all mandatory txs can.
fn can_execute_with_block_base_fee<Transaction: AsRef<TransactionSigned>>(
    list_txs: Vec<(Transaction, bool)>,
    block_base_fee: u128,
) -> bool {
    let mut executable_tx_count = 0u32;
    for (tx, opt) in list_txs.iter().map(|(tx, opt)| (tx.as_ref(), opt)) {
        if tx.max_fee_per_gas() >= block_base_fee {
            executable_tx_count += 1;
        } else if !opt {
            return false;
        }
    }
    executable_tx_count > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::fixed_bytes;
    use reth::primitives::{Transaction, TransactionSigned, TxLegacy};
    use uuid::uuid;

    #[test]
    /// A bundle with a single optional tx paying enough gas should be considered executable
    fn can_execute_single_optional_tx() {
        let needed_base_gas: u128 = 100000;
        let tx = TransactionSignedEcRecovered::from_signed_transaction(
            TransactionSigned {
                transaction: Transaction::Legacy(TxLegacy {
                    gas_price: needed_base_gas,
                    ..Default::default()
                }),
                ..Default::default()
            },
            Address::default(),
        );
        assert!(can_execute_with_block_base_fee(
            vec![(tx, true)],
            needed_base_gas
        ));
    }

    #[test]
    fn test_order_id() {
        let id = "bundle:5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516";
        let parsed = OrderId::from_str(id).unwrap();
        assert_eq!(
            parsed,
            OrderId::Bundle(uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"))
        );
        let serialized = parsed.to_string();
        assert_eq!(serialized, id);
        let fixed_bytes = parsed.fixed_bytes();
        assert_eq!(
            fixed_bytes,
            fixed_bytes!("5d5bf52cac3f57eba3e9fc01b18ca51600000000000000000000000000000000")
        );

        let id = "tx:0x02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5";
        let parsed = OrderId::from_str(id).unwrap();
        assert_eq!(
            parsed,
            OrderId::Tx(fixed_bytes!(
                "02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"
            ))
        );
        let serialized = parsed.to_string();
        assert_eq!(serialized, id);
        let fixed_bytes = parsed.fixed_bytes();
        assert_eq!(
            fixed_bytes,
            fixed_bytes!("02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5")
        );

        let id = "sbundle:0x02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5";
        let parsed = OrderId::from_str(id).unwrap();
        assert_eq!(
            parsed,
            OrderId::ShareBundle(fixed_bytes!(
                "02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"
            ))
        );
        let serialized = parsed.to_string();
        assert_eq!(serialized, id);
        let fixed_bytes = parsed.fixed_bytes();
        assert_eq!(
            fixed_bytes,
            fixed_bytes!("02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5")
        );
    }
}
