//! Order types used as elements for block building.

pub mod built_block;
pub mod evm_inspector;
pub mod fmt;
pub mod mev_boost;
pub mod order_builder;
pub mod order_statistics;
pub mod serialize;
mod test_data_generator;

use alloy_consensus::Transaction as _;
use alloy_eips::{
    eip2718::{Decodable2718, Eip2718Error, Encodable2718},
    eip4844::{Blob, BlobTransactionSidecar, Bytes48, DATA_GAS_PER_BLOB},
    eip7594::BlobTransactionSidecarVariant,
    Typed2718,
};
use alloy_primitives::{keccak256, Address, Bytes, TxHash, B256, U256};
use alloy_rlp::Encodable as _;
use derivative::Derivative;
use evm_inspector::UsedStateTrace;
use integer_encoding::VarInt;
use reth_ethereum_primitives::PooledTransactionVariant;
use reth_primitives::{
    kzg::{BYTES_PER_BLOB, BYTES_PER_COMMITMENT, BYTES_PER_PROOF},
    Recovered, Transaction, TransactionSigned,
};
use reth_primitives_traits::{InMemorySize, SignedTransaction as _, SignerRecoverable};
use reth_transaction_pool::{
    BlobStore, BlobStoreError, EthPooledTransaction, Pool, TransactionOrdering, TransactionPool,
    TransactionValidator,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering, collections::HashMap, fmt::Display, hash::Hash, mem, str::FromStr, sync::Arc,
};
pub use test_data_generator::TestDataGenerator;
use thiserror::Error;
use uuid::Uuid;

use crate::serialize::TxEncoding;

/// Extra metadata for an order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Metadata {
    /// Timestamp at which the order was received.
    pub received_at_timestamp: time::OffsetDateTime,
    // Flag indicating if it's a system order. Defaults to `false`.
    pub is_system: bool,
    /// Order refund identity.
    pub refund_identity: Option<Address>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self::new_received_now()
    }
}

impl Metadata {
    /// Create new metadata with received timestamp set to now.
    pub fn new_received_now() -> Self {
        Self::new(time::OffsetDateTime::now_utc())
    }

    /// Create new metadata.
    pub fn new(received_at_timestamp: time::OffsetDateTime) -> Self {
        Self {
            received_at_timestamp,
            is_system: false,
            refund_identity: None,
        }
    }

    /// Set the `is_system` flag and return the metadata.
    pub fn with_system(mut self, is_system: bool) -> Self {
        self.set_system(is_system);
        self
    }

    /// Set the `is_system` flag.
    pub fn set_system(&mut self, is_system: bool) {
        self.is_system = is_system;
    }

    /// Set the refund identity and return the metadata.
    pub fn with_refund_identity(mut self, refund_identity: Option<Address>) -> Self {
        self.set_refund_identity(refund_identity);
        self
    }

    /// Set the refund identity.
    pub fn set_refund_identity(&mut self, refund_identity: Option<Address>) {
        self.refund_identity = refund_identity;
    }
}

impl InMemorySize for Metadata {
    fn size(&self) -> usize {
        mem::size_of::<time::OffsetDateTime>() + // received_at_timestamp
            mem::size_of::<Option<Address>>() + // refund_identity
            mem::size_of::<bool>() // is_system
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Eq, PartialEq, Clone, Hash, Debug)]
pub struct BundleRefund {
    /// Percent to refund back to the user.
    pub percent: u8,
    /// Address where to refund to.
    pub recipient: Address,
    /// Transaction hash to refund.
    /// This means that part (percent%) of the profit from the execution this txs goes to refund.recipient
    pub tx_hash: TxHash,
    /// Boolean for whether the refund should be delayed.
    pub delayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BundleVersion {
    V1,
    V2,
}

pub const LAST_BUNDLE_VERSION: BundleVersion = BundleVersion::V2;

/// Bundle sent to us usually by a searcher via eth_sendBundle (https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint#eth_sendbundle).
#[derive(Derivative)]
#[derivative(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Bundle {
    pub version: BundleVersion,
    /// None means in the first possible block.
    pub block: Option<u64>,
    pub min_timestamp: Option<u64>,
    pub max_timestamp: Option<u64>,
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    /// A list of tx hashes that are allowed to revert.
    pub reverting_tx_hashes: Vec<B256>,
    /// A list of tx hashes that are allowed to be discarded, but may not revert on chain.
    pub dropping_tx_hashes: Vec<B256>,
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
    pub refund_identity: Option<Address>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,

    /// Bundle refund data.
    pub refund: Option<BundleRefund>,

    /// Unique identifier for a bundle that was set by the sender of the bundle
    pub external_hash: Option<B256>,
}

impl Bundle {
    pub fn can_execute_with_block_base_fee(&self, block_base_fee: u128) -> bool {
        can_execute_with_block_base_fee(self.list_txs(), block_base_fee)
    }

    fn is_tx_optional(&self, hash: &B256) -> bool {
        self.reverting_tx_hashes.contains(hash) || self.dropping_tx_hashes.contains(hash)
    }

    /// BundledTxInfo for all the child txs.
    pub fn nonces(&self) -> Vec<Nonce> {
        let txs = self
            .txs
            .iter()
            .map(|tx| (tx, self.is_tx_optional(&tx.hash())));
        bundle_nonces(txs)
    }

    fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.txs
            .iter()
            .map(|tx| (tx, self.is_tx_optional(&tx.hash())))
            .collect()
    }

    pub fn list_txs_revert(
        &self,
    ) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior)> {
        self.txs
            .iter()
            .map(|tx| {
                let hash = &tx.hash();
                let revert = if self.reverting_tx_hashes.contains(hash) {
                    TxRevertBehavior::AllowedIncluded
                } else if self.dropping_tx_hashes.contains(hash) {
                    TxRevertBehavior::AllowedExcluded
                } else {
                    TxRevertBehavior::NotAllowed
                };
                (tx, revert)
            })
            .collect()
    }

    /// list_txs().len()
    fn list_txs_len(&self) -> usize {
        self.txs.len()
    }

    /// Returns `true` if the provided transaction hash is refundable.
    /// This means that part the profit from this execution goes to the self.refund.recipient
    pub fn is_tx_refundable(&self, hash: &B256) -> bool {
        self.refund.as_ref().is_some_and(|r| r.tx_hash == *hash)
    }

    fn uuid_v1(&mut self) -> Uuid {
        // Block, hash, reverting hashes.
        let mut buff =
            Vec::with_capacity(size_of::<i64>() + 32 + 32 * self.reverting_tx_hashes.len());
        {
            let block = self.block.unwrap_or_default() as i64;
            buff.append(&mut block.encode_var_vec());
        }
        buff.extend_from_slice(self.hash.as_slice());
        self.reverting_tx_hashes.sort();
        for reverted_hash in &self.reverting_tx_hashes {
            buff.extend_from_slice(reverted_hash.as_slice());
        }
        Self::uuid_from_buffer(buff)
    }

    fn uuid_from_buffer(buff: Vec<u8>) -> Uuid {
        let hash = {
            let mut res = [0u8; 16];
            let mut hasher = Sha256::new();
            // We write 16 zeroes to replicate golang hashing behavior.
            hasher.update(res);
            hasher.update(&buff);
            let output = hasher.finalize();
            res.copy_from_slice(&output[0..16]);
            res
        };
        uuid::Builder::from_sha1_bytes(hash).into_uuid()
    }

    fn uuid_v2(&mut self) -> Uuid {
        // Block,min_timestamp,max_timestamp,reverting_tx_hashes.len(),dropping_tx_hashes.len(), hash, reverting hashes,dropping_tx_hashes.
        // If refund present: percent,recipient,tx_hashes.len(),tx_hashes
        let mut buff = Vec::with_capacity(
            5 * size_of::<u64>()
                + 32
                + 32 * (self.reverting_tx_hashes.len()
                    + self.dropping_tx_hashes.len()
                    + if self.refund.is_some() {
                        1usize
                    } else {
                        0usize
                    })
                + size_of::<char>()
                + size_of::<Address>(),
        );
        buff.append(&mut self.block.unwrap_or_default().encode_var_vec());
        buff.append(&mut self.min_timestamp.unwrap_or(u64::MIN).encode_var_vec());
        buff.append(&mut self.max_timestamp.unwrap_or(u64::MAX).encode_var_vec());
        buff.append(&mut (self.reverting_tx_hashes.len() as u64).encode_var_vec());
        buff.append(&mut (self.dropping_tx_hashes.len() as u64).encode_var_vec());
        buff.extend_from_slice(self.hash.as_slice());
        self.reverting_tx_hashes.sort();
        self.dropping_tx_hashes.sort();
        for reverted_hash in &self.reverting_tx_hashes {
            buff.extend_from_slice(reverted_hash.as_slice());
        }
        for dropping_hash in &self.dropping_tx_hashes {
            buff.extend_from_slice(dropping_hash.as_slice());
        }
        if let Some(refund) = &mut self.refund {
            buff.push(refund.percent);
            buff.extend_from_slice(refund.recipient.as_slice());
            // We used to allow multiple hashes and encode the len, we keep the 1 to be backwards compatible.
            buff.append(&mut (1u64).encode_var_vec());
            buff.extend_from_slice(refund.tx_hash.as_slice());
        }
        Self::uuid_from_buffer(buff)
    }

    /// Recalculate bundle hash and uuid.
    /// Hash is computed from child tx hashes + reverting_tx_hashes + dropping_tx_hashes.
    /// @Pending: improve since moving txs from reverting_tx_hashes to dropping_tx_hashes would give the same uuid
    pub fn hash_slow(&mut self) {
        let hash = self
            .txs
            .iter()
            .flat_map(|tx| tx.hash().0.to_vec())
            .collect::<Vec<_>>();
        self.hash = keccak256(hash);
        self.uuid = match self.version {
            BundleVersion::V1 => self.uuid_v1(),
            BundleVersion::V2 => self.uuid_v2(),
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[allow(clippy::large_enum_variant)]
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
///   Refund points to the user txs on the body and has the kickback percentage for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refund {
    /// Index of the ShareBundleInner::body for which this applies.
    pub body_idx: usize,
    /// Percent of the profit going back to the user as kickback.
    pub percent: usize,
}

/// Users can specify how to get kickbacks and this is propagated by the MEV-Share Node to us.
/// We get this configuration as multiple RefundConfigs, then the refunds are paid to the specified addresses in the indicated percentages.
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

    pub fn list_txs_revert(
        &self,
    ) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior)> {
        self.body
            .iter()
            .flat_map(|b| match b {
                ShareBundleBody::Tx(sbundle_tx) => {
                    vec![(&sbundle_tx.tx, sbundle_tx.revert_behavior)]
                }
                ShareBundleBody::Bundle(bundle) => bundle.list_txs_revert(),
            })
            .collect()
    }

    pub fn list_txs_len(&self) -> usize {
        self.body
            .iter()
            .map(|b| match b {
                ShareBundleBody::Tx(_) => 1,
                ShareBundleBody::Bundle(bundle) => bundle.list_txs_len(),
            })
            .sum()
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy, Serialize, Deserialize)]
pub struct ReplacementKey {
    pub id: Uuid,
    /// None means we don't have signer so the identity will be only by uuid.
    /// Source not giving signer risk uuid collision but if uuid is properly generated is almost impossible.
    pub signer: Option<Address>,
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
    inner_bundle: ShareBundleInner,
    /// Cached inner_bundle.list_txs_len()
    list_txs_len: usize,
    pub signer: Option<Address>,
    /// data that uniquely identifies this ShareBundle for update or cancellation
    pub replacement_data: Option<ShareBundleReplacementData>,
    /// Only used internally when we build a virtual (not part of the orderflow) ShareBundle from other orders.
    pub original_orders: Vec<Order>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,
}

impl ShareBundle {
    pub fn new(
        block: u64,
        max_block: u64,
        inner_bundle: ShareBundleInner,
        signer: Option<Address>,
        replacement_data: Option<ShareBundleReplacementData>,
        original_orders: Vec<Order>,
        metadata: Metadata,
    ) -> Self {
        let list_txs_len = inner_bundle.list_txs_len();
        let mut sbundle = Self {
            hash: B256::default(),
            block,
            max_block,
            inner_bundle,
            list_txs_len,
            signer,
            replacement_data,
            original_orders,
            metadata,
        };
        sbundle.hash_slow();
        sbundle
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_fake_hash(
        hash: B256,
        block: u64,
        max_block: u64,
        inner_bundle: ShareBundleInner,
        signer: Option<Address>,
        replacement_data: Option<ShareBundleReplacementData>,
        original_orders: Vec<Order>,
        metadata: Metadata,
    ) -> Self {
        let list_txs_len = inner_bundle.list_txs_len();
        Self {
            hash,
            block,
            max_block,
            inner_bundle,
            list_txs_len,
            signer,
            replacement_data,
            original_orders,
            metadata,
        }
    }

    pub fn with_inner_bundle(self, inner_bundle: ShareBundleInner) -> Self {
        Self::new(
            self.block,
            self.max_block,
            inner_bundle,
            self.signer,
            self.replacement_data,
            self.original_orders,
            self.metadata,
        )
    }

    pub fn inner_bundle(&self) -> &ShareBundleInner {
        &self.inner_bundle
    }
    pub fn can_execute_with_block_base_fee(&self, block_base_fee: u128) -> bool {
        can_execute_with_block_base_fee(self.list_txs(), block_base_fee)
    }

    pub fn list_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.inner_bundle.list_txs()
    }

    pub fn list_txs_revert(
        &self,
    ) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior)> {
        self.inner_bundle.list_txs_revert()
    }

    /// @Pending: optimize by caching (we need to enforce inner_bundle immutability)
    pub fn list_txs_len(&self) -> usize {
        self.list_txs_len
    }

    /// BundledTxInfo for all the child txs
    pub fn nonces(&self) -> Vec<Nonce> {
        bundle_nonces(self.inner_bundle.list_txs().into_iter())
    }

    pub fn flatten_txs(&self) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, bool)> {
        self.inner_bundle.list_txs()
    }

    // Recalculate bundle hash.
    fn hash_slow(&mut self) {
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

#[derive(Error, Debug, derive_more::From)]
pub enum TxWithBlobsCreateError {
    #[error("Failed to decode transaction, error: {0}")]
    FailedToDecodeTransaction(Eip2718Error),
    #[error("Invalid transaction signature")]
    InvalidTransactionSignature,
    #[error("UnexpectedError")]
    UnexpectedError,
    /// This error is generated when we fail (like FailedToDecodeTransaction) parsing in TxEncoding::WithBlobData mode (Network encoding) but the header looks
    /// like the beginning of an ethereum mainnet Canonical encoding 4484 tx.
    /// To avoid consuming resources the generation of this error might not be perfect but helps 99% of the time.
    #[error("Failed to decode transaction, error: {0}. It probably is a 4484 canonical tx.")]
    FailedToDecodeTransactionProbablyIs4484Canonical(alloy_rlp::Error),
    #[error("Tried to create an EIP4844 transaction without a blob")]
    Eip4844MissingBlobSidecar,
    #[error("Tried to create a non-EIP4844 transaction while passing blobs")]
    BlobsMissingEip4844,
    #[error("BlobStoreError: {0}")]
    BlobStore(BlobStoreError),
}

trait FakeSidecar {
    fn fake_sidecar(blob_versioned_hashes_len: usize) -> BlobTransactionSidecar;
}

impl FakeSidecar for BlobTransactionSidecar {
    fn fake_sidecar(blob_versioned_hashes_len: usize) -> BlobTransactionSidecar {
        let mut fake_sidecar = BlobTransactionSidecar::default();
        for _ in 0..blob_versioned_hashes_len {
            fake_sidecar.blobs.push(Blob::from([0u8; BYTES_PER_BLOB]));
            fake_sidecar
                .commitments
                .push(Bytes48::from([0u8; BYTES_PER_COMMITMENT]));
            fake_sidecar
                .proofs
                .push(Bytes48::from([0u8; BYTES_PER_PROOF]));
        }
        fake_sidecar
    }
}

/// First idea to handle blobs, might change.
/// Don't like the fact that blobs_sidecar exists no matter if Recovered<TransactionSigned> contains a non blob tx.
/// Great effort was put in avoiding simple access to the internal tx so we don't accidentally leak information on logs (particularly the tx sign).
#[derive(Derivative)]
#[derivative(Clone, PartialEq, Eq)]
pub struct TransactionSignedEcRecoveredWithBlobs {
    tx: Recovered<TransactionSigned>,
    /// Will have a non empty BlobTransactionSidecarVariant if Recovered<TransactionSigned> is 4844
    pub blobs_sidecar: Arc<BlobTransactionSidecarVariant>,

    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub metadata: Metadata,
}

impl AsRef<TransactionSigned> for TransactionSignedEcRecoveredWithBlobs {
    fn as_ref(&self) -> &TransactionSigned {
        &self.tx
    }
}

impl Typed2718 for TransactionSignedEcRecoveredWithBlobs {
    fn ty(&self) -> u8 {
        self.tx.ty()
    }
}

impl Encodable2718 for TransactionSignedEcRecoveredWithBlobs {
    fn type_flag(&self) -> Option<u8> {
        self.tx.type_flag()
    }

    fn encode_2718_len(&self) -> usize {
        self.tx.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.tx.encode_2718(out)
    }
}

/// Custom fmt to avoid leaking information.
impl std::fmt::Debug for TransactionSignedEcRecoveredWithBlobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TransactionSignedEcRecoveredWithBlobs {{ hash: {} }}",
            self.hash(),
        )
    }
}

impl TryFrom<Recovered<PooledTransactionVariant>> for TransactionSignedEcRecoveredWithBlobs {
    type Error = TxWithBlobsCreateError;

    fn try_from(value: Recovered<PooledTransactionVariant>) -> Result<Self, Self::Error> {
        let (tx, signer) = value.into_parts();

        match tx {
            PooledTransactionVariant::Legacy(_)
            | PooledTransactionVariant::Eip2930(_)
            | PooledTransactionVariant::Eip1559(_)
            | PooledTransactionVariant::Eip7702(_) => {
                let tx_signed = TransactionSigned::from(tx);
                TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx_signed.with_signer(signer))
            }
            PooledTransactionVariant::Eip4844(blob_tx) => {
                let (blob_tx, signature, hash) = blob_tx.into_parts();
                let (blob_tx, sidecar) = blob_tx.into_parts();
                let tx_signed = TransactionSigned::new_unchecked(
                    Transaction::Eip4844(blob_tx),
                    signature,
                    hash,
                );
                Ok(TransactionSignedEcRecoveredWithBlobs {
                    tx: tx_signed.with_signer(signer),
                    blobs_sidecar: Arc::new(sidecar),
                    metadata: Metadata::default(),
                })
            }
        }
    }
}

impl TransactionSignedEcRecoveredWithBlobs {
    /// Create new with an optional blob sidecar.
    ///
    /// Warning: It is the caller's responsibility to check if a tx has blobs.
    /// This fn will return an Err if it is passed an eip4844 without blobs,
    /// or blobs without an eip4844.
    pub fn new(
        tx: Recovered<TransactionSigned>,
        blob_sidecar: Option<BlobTransactionSidecarVariant>,
        metadata: Option<Metadata>,
    ) -> Result<Self, TxWithBlobsCreateError> {
        // Check for an eip4844 tx passed without blobs
        if tx.inner().blob_versioned_hashes().is_some() && blob_sidecar.is_none() {
            Err(TxWithBlobsCreateError::Eip4844MissingBlobSidecar)
        // Check for a non-eip4844 tx passed with blobs
        } else if blob_sidecar.is_some() && tx.inner().blob_versioned_hashes().is_none() {
            Err(TxWithBlobsCreateError::BlobsMissingEip4844)
        // Groovy!
        } else {
            let sidecar = blob_sidecar.unwrap_or(Self::default_blob_sidecar());
            Ok(Self {
                tx,
                blobs_sidecar: Arc::new(sidecar),
                metadata: metadata.unwrap_or_default(),
            })
        }
    }

    /// Estimated length used to measure block space so avoid reaching EIP-7934 limit.
    pub fn length_eip7934(&self) -> usize {
        self.tx.inner().length()
    }

    pub fn space_needed(&self) -> BlockSpace {
        BlockSpace::new(
            self.tx.gas_limit(),
            self.length_eip7934(),
            self.blobs_gas_used(),
        )
    }

    pub fn blobs_len(&self) -> usize {
        match self.blobs_sidecar.as_ref() {
            BlobTransactionSidecarVariant::Eip4844(sidecar) => sidecar.blobs.len(),
            BlobTransactionSidecarVariant::Eip7594(sidecar) => sidecar.blobs.len(),
        }
    }

    pub fn blobs_gas_used(&self) -> u64 {
        self.blobs_len() as u64 * DATA_GAS_PER_BLOB
    }

    /// For when we don't have a sidecar. Not sure if Eip4844 is the right choice.
    fn default_blob_sidecar() -> BlobTransactionSidecarVariant {
        BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar::default())
    }

    /// Shorthand for `new(tx, None, None)`
    pub fn new_no_blobs(tx: Recovered<TransactionSigned>) -> Result<Self, TxWithBlobsCreateError> {
        Self::new(tx, None, None)
    }

    /// Try to create a [`TransactionSignedEcRecoveredWithBlobs`] from a
    /// [`Recovered<TransactionSigned>`] and reth pool.
    ///
    /// The pool is required because [`Recovered<TransactionSigned>`] on its
    /// own does not contain blob information, it is required to fetch the blob.
    ///
    /// Unfortunately we need to pass the entire pool, because the blob store
    /// is not part of the pool's public api.
    pub fn try_from_tx_without_blobs_and_pool<V, T, S>(
        tx: Recovered<TransactionSigned>,
        pool: Pool<V, T, S>,
    ) -> Result<Self, TxWithBlobsCreateError>
    where
        V: TransactionValidator<Transaction = EthPooledTransaction>,
        T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
        S: BlobStore,
    {
        /* At aprox 2025-08 get_blob was failing so we switched to get_all_blobs.
        let blob_sidecar = pool
        .get_blob(*tx.inner().hash())?
        .map(|b| b.as_ref().clone());*/
        let mut blobs = pool.get_all_blobs(vec![*tx.inner().hash()])?;
        let blob_sidecar = blobs.pop().map(|(_, arc)| arc.as_ref().clone());
        Self::new(tx, blob_sidecar, None)
    }

    /// Creates a Self with empty blobs sidecar. No consistency check is performed!
    pub fn new_for_testing(tx: Recovered<TransactionSigned>) -> Self {
        Self {
            tx,
            blobs_sidecar: Arc::new(Self::default_blob_sidecar()),
            metadata: Default::default(),
        }
    }

    pub fn hash(&self) -> TxHash {
        *self.tx.inner().hash()
    }

    pub fn signer(&self) -> Address {
        self.tx.signer()
    }

    pub fn to(&self) -> Option<Address> {
        self.tx.to()
    }

    pub fn nonce(&self) -> u64 {
        self.tx.nonce()
    }

    pub fn value(&self) -> U256 {
        self.tx.value()
    }

    /// USE CAREFULLY since this exposes the signed tx.
    pub fn internal_tx_unsecure(&self) -> &Recovered<TransactionSigned> {
        &self.tx
    }

    /// USE CAREFULLY since this exposes the signed tx.
    pub fn into_internal_tx_unsecure(self) -> Recovered<TransactionSigned> {
        self.tx
    }

    /// Encodes the "raw" canonical format of transaction (NOT the one used in `eth_sendRawTransaction`) BLOB DATA IS NOT ENCODED.
    /// I intentsionally omitted the version with blob data since we don't use it and may lead to confusions/bugs.
    /// USE CAREFULLY since this exposes the signed tx.
    pub fn envelope_encoded_no_blobs(&self) -> Bytes {
        let mut buf = Vec::new();
        self.tx.encode_2718(&mut buf);
        buf.into()
    }
}

/// Trait alias to lookup the signer of a tx by its hash.
pub trait SignerLookup: Fn(B256) -> Option<Address> {}
impl<T: Fn(B256) -> Option<Address>> SignerLookup for T {}

/// Raw transaction bytes along with:
/// - the encoding used (with or without blob data)
/// - an optional signer lookup to avoid signature recovery when we already know the signer
pub struct RawTransactionDecodable<T: SignerLookup> {
    pub raw: Bytes,
    pub encoding: TxEncoding,
    signer_lookup: Option<T>,
}

impl RawTransactionDecodable<fn(B256) -> Option<Address>> {
    pub fn new(raw: Bytes, encoding: TxEncoding) -> Self {
        Self {
            raw,
            encoding,
            signer_lookup: Option::<fn(B256) -> Option<Address>>::None,
        }
    }
}

impl<T: SignerLookup> RawTransactionDecodable<T> {
    /// Allows to set a custom signer lookup.
    pub fn with_signer_lookup<U: SignerLookup>(self, lookup: U) -> RawTransactionDecodable<U> {
        RawTransactionDecodable {
            raw: self.raw,
            encoding: self.encoding,
            signer_lookup: Some(lookup),
        }
    }

    /// Decodes the raw transaction bytes into a [`TransactionSignedEcRecoveredWithBlobs`].
    ///
    /// If the encoding is `TxEncoding::WithBlobData`, the blob data is expected to be
    /// present in the raw bytes. Otherwise, fake blob data is generated.
    pub fn decode_enveloped(
        &self,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, TxWithBlobsCreateError> {
        match self.encoding {
            TxEncoding::WithBlobData => self.decode_enveloped_with_real_blobs(),
            TxEncoding::NoBlobData => self.decode_enveloped_with_fake_blobs(),
        }
    }

    /// Decodes the "raw" format of transaction (e.g. `eth_sendRawTransaction`) with the blob data (network format)
    fn decode_enveloped_with_real_blobs(
        &self,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, TxWithBlobsCreateError> {
        let raw_tx = &mut self.raw.as_ref();

        let pooled_tx = PooledTransactionVariant::decode_2718(raw_tx)
            .map_err(TxWithBlobsCreateError::FailedToDecodeTransaction)?;

        let signer = self
            .signer_lookup
            .as_ref()
            .and_then(|sl| sl(*pooled_tx.tx_hash()))
            .or_else(|| pooled_tx.recover_signer().ok())
            .ok_or(TxWithBlobsCreateError::InvalidTransactionSignature)?;

        Recovered::<PooledTransactionVariant>::new_unchecked(pooled_tx, signer).try_into()
    }

    /// Decodes the "raw" canonical format of transaction (NOT the one used in `eth_sendRawTransaction`) generating fake blob data for backtesting
    fn decode_enveloped_with_fake_blobs(
        &self,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, TxWithBlobsCreateError> {
        let decoded = TransactionSigned::decode_2718(&mut self.raw.as_ref())
            .map_err(TxWithBlobsCreateError::FailedToDecodeTransaction)?;

        let hash = *decoded.hash();

        let signer = self
            .signer_lookup
            .as_ref()
            .and_then(|sl| sl(hash))
            .or_else(|| decoded.recover_signer().ok())
            .ok_or(TxWithBlobsCreateError::InvalidTransactionSignature)?;

        let tx = Recovered::new_unchecked(decoded, signer);
        let hashes_len = tx.blob_versioned_hashes().map_or(0, |hashes| hashes.len());
        let fake_sidecar = BlobTransactionSidecar::fake_sidecar(hashes_len);

        Ok(TransactionSignedEcRecoveredWithBlobs {
            tx,
            blobs_sidecar: Arc::new(BlobTransactionSidecarVariant::Eip4844(fake_sidecar)),
            metadata: Metadata::default(),
        })
    }
}

impl std::hash::Hash for TransactionSignedEcRecoveredWithBlobs {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        //This is enough to identify the tx
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

impl InMemorySize for MempoolTx {
    fn size(&self) -> usize {
        self.tx_with_blobs.tx.inner().size()
            + self.tx_with_blobs.blobs_sidecar.size()
            + self.tx_with_blobs.metadata.size()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShareBundleReplacementKey(ReplacementKey);
impl ShareBundleReplacementKey {
    pub fn new(id: Uuid, signer: Address) -> Self {
        Self(ReplacementKey {
            id,
            signer: Some(signer),
        })
    }

    pub fn key(&self) -> ReplacementKey {
        self.0
    }
}

/// Uniquely identifies a replaceable bundle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundleReplacementKey(ReplacementKey);
impl BundleReplacementKey {
    pub fn new(id: Uuid, signer: Option<Address>) -> Self {
        Self(ReplacementKey { id, signer })
    }
    pub fn key(&self) -> ReplacementKey {
        self.0
    }
}

/// General type for both BundleReplacementKey and ShareBundleReplacementKey
/// Even although BundleReplacementKey and ShareBundleReplacementKey have the same info they are kept
/// as different types to avoid bugs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            Order::Tx(tx) => OrderId::Tx(tx.tx_with_blobs.hash()),
            Order::ShareBundle(bundle) => OrderId::ShareBundle(bundle.hash),
        }
    }

    pub fn external_bundle_hash(&self) -> Option<B256> {
        match self {
            Order::Bundle(b) => b.external_hash,
            _ => None,
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

    pub fn list_txs_revert(
        &self,
    ) -> Vec<(&TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior)> {
        match self {
            Order::Bundle(bundle) => bundle.list_txs_revert(),
            Order::Tx(tx) => vec![(&tx.tx_with_blobs, TxRevertBehavior::AllowedIncluded)],
            Order::ShareBundle(bundle) => bundle.list_txs_revert(),
        }
    }

    /// list_txs().len()
    pub fn list_txs_len(&self) -> usize {
        match self {
            Order::Bundle(bundle) => bundle.list_txs_len(),
            Order::Tx(_) => 1,
            Order::ShareBundle(bundle) => bundle.list_txs_len(),
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
        self.list_txs().iter().any(|(tx, _)| tx.blobs_len() > 0)
    }

    pub fn target_block(&self) -> Option<u64> {
        match self {
            Order::Bundle(bundle) => bundle.block,
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

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct ProfitInfo {
    /// profit as coinbase delta after executing an Order
    coinbase_profit: U256,
    /// This is computed as coinbase_profit/gas_used so it includes not only gas tip but also payments made directly to coinbase
    mev_gas_price: U256,
}

impl ProfitInfo {
    pub fn new(coinbase_profit: U256, gas_used: u64) -> Self {
        let mev_gas_price = if gas_used != 0 {
            coinbase_profit / U256::from(gas_used)
        } else {
            U256::ZERO
        };
        Self {
            coinbase_profit,
            mev_gas_price,
        }
    }

    /// For testing specific values ignoring gas.
    pub fn new_test(coinbase_profit: U256, mev_gas_price: U256) -> Self {
        Self {
            coinbase_profit,
            mev_gas_price,
        }
    }

    pub fn coinbase_profit(&self) -> U256 {
        self.coinbase_profit
    }

    pub fn mev_gas_price(&self) -> U256 {
        self.mev_gas_price
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct SimValue {
    /// ProfitInfo considering profit from all txs on the s/bundles.
    full_profit_info: ProfitInfo,
    /// ProfitInfo considering profit only from non mempool txs on the s/bundles.
    /// For mempool orders it should match ProfitInfo
    non_mempool_profit_info: ProfitInfo,
    space_used: BlockSpace,
    /// Kickbacks paid during simulation as (receiver, amount)
    paid_kickbacks: Vec<(Address, U256)>,
}

impl SimValue {
    pub fn new(
        // full profit
        full_coinbase_profit: U256,
        // for s/bundles profit from non-mempool txs.
        non_mempool_coinbase_profit: U256,
        space_used: BlockSpace,
        paid_kickbacks: Vec<(Address, U256)>,
    ) -> Self {
        Self {
            full_profit_info: ProfitInfo::new(full_coinbase_profit, space_used.gas),
            non_mempool_profit_info: ProfitInfo::new(non_mempool_coinbase_profit, space_used.gas),
            space_used,
            paid_kickbacks,
        }
    }

    /// For testing specific coinbase_profit/mev_gas_price values ignoring gas.
    /// coinbase_profit is the same for full_profit_info/non_mempool_profit_info
    pub fn new_test_no_gas(coinbase_profit: U256, mev_gas_price: U256) -> Self {
        Self {
            full_profit_info: ProfitInfo::new_test(coinbase_profit, mev_gas_price),
            non_mempool_profit_info: ProfitInfo::new_test(coinbase_profit, mev_gas_price),
            ..Default::default()
        }
    }

    pub fn new_test(full_coinbase_profit: U256, non_mempool_profit: U256, gas_used: u64) -> Self {
        Self {
            full_profit_info: ProfitInfo::new(full_coinbase_profit, gas_used),
            non_mempool_profit_info: ProfitInfo::new(non_mempool_profit, gas_used),
            space_used: BlockSpace::new(gas_used, 0, 0),
            ..Default::default()
        }
    }

    pub fn full_profit_info(&self) -> &ProfitInfo {
        &self.full_profit_info
    }

    pub fn non_mempool_profit_info(&self) -> &ProfitInfo {
        &self.non_mempool_profit_info
    }

    pub fn gas_used(&self) -> u64 {
        self.space_used.gas
    }

    pub fn blob_gas_used(&self) -> u64 {
        self.space_used.blob_gas
    }

    pub fn paid_kickbacks(&self) -> &Vec<(Address, U256)> {
        &self.paid_kickbacks
    }

    pub fn with_kickbacks(mut self, kickbacks: Vec<(Address, U256)>) -> Self {
        self.paid_kickbacks = kickbacks;
        self
    }
}

/// Order simulated (usually on top of block) + SimValue
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedOrder {
    pub order: Order,
    pub sim_value: SimValue,
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

    /// Returns tx hash if the order is mempool tx
    pub fn tx_hash(&self) -> Option<B256> {
        match self {
            Self::Tx(hash) => Some(*hash),
            _ => None,
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
            Self::Tx(hash) => write!(f, "tx:{hash:?}"),
            Self::Bundle(uuid) => write!(f, "bundle:{uuid:?}"),
            Self::ShareBundle(hash) => write!(f, "sbundle:{hash:?}"),
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

/// Models consumed/reserved space on a block to be able to insert payout tx when finished filling the block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BlockSpace {
    pub gas: u64,
    /// EIP-7934 limits the size of the final rlp block.
    /// Estimation of the sum of the rlp txs sizes.
    pub rlp_length: usize,
    pub blob_gas: u64,
}

impl BlockSpace {
    pub fn new(gas: u64, rlp_length: usize, blob_gas: u64) -> Self {
        Self {
            gas,
            rlp_length,
            blob_gas,
        }
    }

    pub const ZERO: Self = Self {
        gas: 0,
        rlp_length: 0,
        blob_gas: 0,
    };
}

impl std::ops::AddAssign for BlockSpace {
    fn add_assign(&mut self, other: Self) {
        self.gas += other.gas;
        self.rlp_length += other.rlp_length;
        self.blob_gas += other.blob_gas;
    }
}

impl std::ops::Add for BlockSpace {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            gas: self.gas + other.gas,
            rlp_length: self.rlp_length + other.rlp_length,
            blob_gas: self.blob_gas + other.blob_gas,
        }
    }
}

impl std::ops::SubAssign for BlockSpace {
    fn sub_assign(&mut self, other: Self) {
        self.gas = self.gas.checked_sub(other.gas).unwrap();
        self.rlp_length = self.rlp_length.checked_sub(other.rlp_length).unwrap();
        self.blob_gas = self.blob_gas.checked_sub(other.blob_gas).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{fixed_bytes, Signature};
    use reth_primitives::{Transaction, TransactionSigned};
    use uuid::uuid;

    #[test]
    /// A bundle with a single optional tx paying enough gas should be considered executable
    fn can_execute_single_optional_tx() {
        let needed_base_gas: u128 = 100000;
        let tx = Recovered::new_unchecked(
            TransactionSigned::new_unchecked(
                Transaction::Legacy(TxLegacy {
                    gas_price: needed_base_gas,
                    ..Default::default()
                }),
                Signature::test_signature(),
                Default::default(),
            ),
            Address::default(),
        );
        assert!(can_execute_with_block_base_fee(
            vec![(tx, true)],
            needed_base_gas
        ));
    }

    #[test]
    fn test_order_id_json() {
        let id = OrderId::Tx(fixed_bytes!(
            "02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"
        ));
        let serialized = serde_json::to_string(&id).unwrap();
        assert_eq!(
            serialized,
            r#"{"Tx":"0x02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"}"#
        );

        let id = OrderId::Bundle(uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"));
        let serialized = serde_json::to_string(&id).unwrap();
        assert_eq!(
            serialized,
            r#"{"Bundle":"5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"}"#
        );

        let id = OrderId::ShareBundle(fixed_bytes!(
            "02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"
        ));
        let serialized = serde_json::to_string(&id).unwrap();
        assert_eq!(
            serialized,
            r#"{"ShareBundle":"0x02e81e3cee67f25203db1178fb11070fcdace65c4eef80daa4037d9b49f011f5"}"#
        );
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
