use super::{
    Bundle, BundleRefund, BundleReplacementData, BundleReplacementKey, BundleVersion, MempoolTx,
    Order, RawTransactionDecodable, Refund, RefundConfig, ShareBundle, ShareBundleBody,
    ShareBundleInner, ShareBundleReplacementData, ShareBundleReplacementKey, ShareBundleTx,
    TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior, TxWithBlobsCreateError,
    LAST_BUNDLE_VERSION,
};
use alloy_consensus::constants::EIP4844_TX_TYPE_ID;
use alloy_eips::eip2718::Eip2718Error;
use alloy_primitives::{Address, Bytes, TxHash, B256, U64};
use alloy_rlp::{Buf, Header};
use derivative::Derivative;
use reth_chainspec::MAINNET;
use serde::{Deserialize, Deserializer, Serialize};
use serde_with::serde_as;
use thiserror::Error;
use tracing::error;
use uuid::Uuid;

/// Encoding mode for raw transactions (https://eips.ethereum.org/EIPS/eip-4844)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxEncoding {
    /// Canonical encoding, for 4844 is only tx_payload_body
    NoBlobData,
    /// Network encoding, for 4844 includes rlp([tx_payload_body, blobs, commitments, proofs])
    /// This mode is used un eth_sendRawTransaction
    WithBlobData,
}

impl TxEncoding {
    pub fn decode(
        &self,
        raw_tx: Bytes,
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, TxWithBlobsCreateError> {
        // This clone is supposed to be cheap
        let res = RawTransactionDecodable::new(raw_tx.clone(), *self).decode_enveloped();

        match self {
            TxEncoding::NoBlobData => res,
            TxEncoding::WithBlobData => {
                if let Err(TxWithBlobsCreateError::FailedToDecodeTransaction(
                    Eip2718Error::RlpError(err),
                )) = res
                {
                    if Self::looks_like_canonical_blob_tx(raw_tx) {
                        return Err(TxWithBlobsCreateError::FailedToDecodeTransactionProbablyIs4484Canonical(
                    err,
                ));
                    }
                }
                res
            }
        }
    }

    fn looks_like_canonical_blob_tx(raw_tx: Bytes) -> bool {
        // For full check we could call TransactionSigned::decode_enveloped and fully try to decode it is way more expensive.
        // We expect EIP4844_TX_TYPE_ID + rlp(chainId = 01,.....)
        let mut tx_slice = raw_tx.as_ref();
        if let Some(tx_type) = tx_slice.first() {
            if *tx_type == EIP4844_TX_TYPE_ID {
                tx_slice.advance(1);
                if let Ok(outer_header) = Header::decode(&mut tx_slice) {
                    if outer_header.list {
                        if let Some(chain_id) = tx_slice.first() {
                            return (*chain_id as u64) == MAINNET.chain().id();
                        }
                    }
                }
            }
        }
        false
    }
}

fn deserialize_vec_from_null_or_string<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    // Option::deserialize handles null.S
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

fn deserialize_vec_b256_from_null_or_string<'de, D>(deserializer: D) -> Result<Vec<B256>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_vec_from_null_or_string(deserializer)
}

/// Struct to de/serialize JSON bundles data from bundles APIs and from/db, except transactions.
/// To be used long with `RawBundle<T>`.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
#[derivative(PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RawBundleMetadata {
    pub version: Option<String>,
    /// blockNumber (Optional) `String`, a hex encoded block number for which this bundle is valid
    /// on. If nil or 0, blockNumber will default to the current pending block
    pub block_number: Option<U64>,
    /// revertingTxHashes (Optional) `Array[String]`, A list of tx hashes that are allowed to
    /// revert
    #[serde(default, deserialize_with = "deserialize_vec_b256_from_null_or_string")]
    pub reverting_tx_hashes: Vec<B256>,
    /// droppingTxHashes (Optional) `Array[String]` A list of tx hashes that are allowed to be
    /// discarded, but may not revert on chain.
    /// Only for version None or >= v2
    #[serde(default, deserialize_with = "deserialize_vec_b256_from_null_or_string")]
    pub dropping_tx_hashes: Vec<B256>,
    /// a UUID v4 that can be used to replace or cancel this
    /// bundle
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<Uuid>,
    /// Same as replacement_uuid since the API change from builder to builder and we want to be compatible with all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<Uuid>,
    /// Address of the bundle sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_address: Option<Address>,
    // refundIdentity (Optional) `String`, Address that BuilderNet refunds should be sent to instead of the bundle signer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refund_identity: Option<Address>,
    /// minTimestamp (Optional) `Number`, the minimum timestamp for which this bundle is valid, in
    /// seconds since the unix epoch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_timestamp: Option<u64>,
    /// maxTimestamp (Optional) `Number`, the maximum timestamp for which this bundle is valid, in
    /// seconds since the unix epoch
    /// A value of 0 means it is unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_timestamp: Option<u64>,
    /// See [`BundleReplacementData`] sequence_number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_nonce: Option<u64>,

    /// refundPercent (Optional) `Number`, percent to refund back to the user.
    /// Only for version None or >= v2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refund_percent: Option<u8>,
    /// refundRecipient (Optional) `Address`, address of the user where to refund to. If
    /// refundPercent is set and refundRecipient is not, the whole bundle will be discarded.
    /// Only for version None or >= v2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refund_recipient: Option<Address>,
    /// refundTxHashes (Optional) `Array[String]`, A list of tx hashes from which the refund is
    /// calculated. Defaults to final transaction in the bundle if list is not specified/empty.
    /// Only for version None or >= v2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refund_tx_hashes: Option<Vec<TxHash>>,
    /// delayedRefund (Optional) `Boolean`, A flag indicating whether the refund should be delayed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delayed_refund: Option<bool>,
    /// bundleHash, externally set unique identifier for the bundle
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_hash: Option<B256>,
}

impl RawBundleMetadata {
    /// consistency checks on raw data.
    /// uuid takes priority over replacement_nonce
    fn decode_replacement_data(
        &self,
    ) -> Result<Option<BundleReplacementData>, RawBundleConvertError> {
        let uuid = self.uuid.or(self.replacement_uuid);

        match uuid {
            Some(uuid) => {
                let replacement_nonce = self
                    .replacement_nonce
                    .ok_or(RawBundleConvertError::IncorrectReplacementData)?;

                let signer = self
                    .signing_address
                    .ok_or(RawBundleConvertError::IncorrectReplacementData)?;

                Ok(Some(BundleReplacementData {
                    key: BundleReplacementKey::new(uuid, Some(signer)),
                    sequence_number: replacement_nonce,
                }))
            }
            None => Ok(None),
        }
    }

    fn decode_version(&self) -> Result<BundleVersion, RawBundleConvertError> {
        if let Some(version) = self.version.as_deref() {
            match version {
                BUNDLE_VERSION_V1 => Ok(BundleVersion::V1),
                BUNDLE_VERSION_V2 => Ok(BundleVersion::V2),
                _ => Err(RawBundleConvertError::UnsupportedVersion(
                    version.to_string(),
                )),
            }
        } else {
            Ok(LAST_BUNDLE_VERSION)
        }
    }

    /// Validates if all fields are valid for the version.
    fn validate_fields(&self, version: BundleVersion) -> Result<(), RawBundleConvertError> {
        match version {
            BundleVersion::V1 => {
                // Fields add on V2
                if !self.dropping_tx_hashes.is_empty() {
                    return Err(RawBundleConvertError::FieldNotSupportedByVersion(
                        "dropping_tx_hashes".to_owned(),
                        version,
                    ));
                }
                if self.refund_percent.is_some() {
                    return Err(RawBundleConvertError::FieldNotSupportedByVersion(
                        "refund_percent".to_owned(),
                        version,
                    ));
                }
                if self.refund_recipient.is_some() {
                    return Err(RawBundleConvertError::FieldNotSupportedByVersion(
                        "refund_recipient".to_owned(),
                        version,
                    ));
                }
                if self.refund_tx_hashes.is_some() {
                    return Err(RawBundleConvertError::FieldNotSupportedByVersion(
                        "refund_tx_hashes".to_owned(),
                        version,
                    ));
                }
                if self.delayed_refund.is_some() {
                    return Err(RawBundleConvertError::FieldNotSupportedByVersion(
                        "delayed_refund".to_owned(),
                        version,
                    ));
                }
                Ok(())
            }
            BundleVersion::V2 => Ok(()),
        }
    }
}

/// Struct to de/serialize json Bundles from bundles APIs and from/db.
/// Does not assume a particular format on txs.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
#[derivative(PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RawBundle {
    #[serde(flatten)]
    pub metadata: RawBundleMetadata,
    /// txs `Array[String]`, A list of signed transactions to execute in an atomic bundle, list can
    /// be empty for bundle cancellations.
    #[serde(default, deserialize_with = "deserialize_vec_from_null_or_string")]
    pub txs: Vec<Bytes>,
}

/// A [`RawBundle`] with transactions already decoded and recovered.
struct RawBundleRecovered {
    pub metadata: RawBundleMetadata,
    /// txs `Array[String]`, A list of signed transactions to execute in an atomic bundle, list can
    /// be empty for bundle cancellations.
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
}

#[derive(Error, Debug)]
pub enum RawBundleConvertError {
    #[error("Failed to decode transaction, idx: {0}, error: {1}")]
    FailedToDecodeTransaction(usize, TxWithBlobsCreateError),
    #[error("Incorrect replacement data")]
    IncorrectReplacementData,
    #[error("Blobs not supported by RawBundle")]
    BlobsNotSupported,
    #[error("Invalid refund percent {0}")]
    InvalidRefundPercent(u8),
    #[error("Empty bundle with on uuid")]
    EmptyBundle,
    #[error("Found cancel on decode_new_bundle")]
    FoundCancelExpectingBundle,
    #[error("Unsupported version {0}")]
    UnsupportedVersion(String),
    #[error("Field {0} not supported by version {1:?}")]
    FieldNotSupportedByVersion(String, BundleVersion),
    #[error("More than one refund tx hash not supported")]
    MoreThanOneRefundTxHash,
}

/// Since we use the same API (eth_sendBundle) to get new bundles and also to cancel them we need this struct.
#[allow(clippy::large_enum_variant)]
pub enum RawBundleDecodeResult {
    NewBundle(Bundle),
    CancelBundle(BundleReplacementData),
}

pub const BUNDLE_VERSION_V1: &str = "v1";
pub const BUNDLE_VERSION_V2: &str = "v2";

impl RawBundle {
    /// Same as decode but fails on cancel
    pub fn decode_new_bundle(self, encoding: TxEncoding) -> Result<Bundle, RawBundleConvertError> {
        let decode_res = self.decode(encoding)?;
        match decode_res {
            RawBundleDecodeResult::NewBundle(b) => Ok(b),
            RawBundleDecodeResult::CancelBundle(_) => {
                Err(RawBundleConvertError::FoundCancelExpectingBundle)
            }
        }
    }

    pub fn decode(
        self,
        encoding: TxEncoding,
    ) -> Result<RawBundleDecodeResult, RawBundleConvertError> {
        self.decode_inner(encoding, Option::<fn(B256) -> Option<Address>>::None)
    }

    pub fn decode_with_signer_lookup(
        self,
        encoding: TxEncoding,
        signer_lookup: impl Fn(B256) -> Option<Address>,
    ) -> Result<RawBundleDecodeResult, RawBundleConvertError> {
        self.decode_inner(encoding, Some(signer_lookup))
    }

    fn decode_inner(
        mut self,
        encoding: TxEncoding,
        signer_lookup: Option<impl Fn(B256) -> Option<Address>>,
    ) -> Result<RawBundleDecodeResult, RawBundleConvertError> {
        let replacement_data = self.metadata.decode_replacement_data()?; // Check for cancellation
        if self.txs.is_empty() {
            match replacement_data {
                Some(replacement_data) => {
                    return Ok(RawBundleDecodeResult::CancelBundle(replacement_data))
                }
                None => return Err(RawBundleConvertError::EmptyBundle),
            }
        }
        let version = self.metadata.decode_version()?;
        self.metadata.validate_fields(version)?;

        self.metadata.reverting_tx_hashes.sort();
        self.metadata.dropping_tx_hashes.sort();

        let recovered_txs = std::mem::take(&mut self.txs)
            .into_iter()
            .enumerate()
            .map(|(idx, tx)| {
                let decodable = RawTransactionDecodable {
                    raw: tx,
                    encoding,
                    signer_lookup: signer_lookup.as_ref(),
                };
                decodable
                    .decode_enveloped()
                    .map_err(|e| RawBundleConvertError::FailedToDecodeTransaction(idx, e))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut recovered_bundle = self.into_recovered(recovered_txs);

        let refund = recovered_bundle.parse_refund()?;
        let block = recovered_bundle
            .metadata
            .block_number
            .unwrap_or_default()
            .to();

        let RawBundleRecovered { metadata, txs } = recovered_bundle;
        let mut bundle = Bundle {
            block: if block != 0 { Some(block) } else { None },
            txs,
            reverting_tx_hashes: metadata.reverting_tx_hashes,
            hash: Default::default(),
            uuid: Default::default(),
            replacement_data,
            // we assume that 0 timestamp is the same as timestamp not set
            min_timestamp: metadata.min_timestamp,
            max_timestamp: metadata.max_timestamp.filter(|t| *t != 0),
            signer: metadata.signing_address,
            refund_identity: metadata.refund_identity,
            metadata: Default::default(),
            dropping_tx_hashes: metadata.dropping_tx_hashes,
            refund,
            version,
            external_hash: metadata.bundle_hash,
        };
        bundle.hash_slow();
        Ok(RawBundleDecodeResult::NewBundle(bundle))
    }

    /// See [TransactionSignedEcRecoveredWithBlobs::envelope_encoded_no_blobs]
    pub fn encode_no_blobs(value: Bundle) -> Self {
        let replacement_uuid = value.replacement_data.as_ref().map(|r| r.key.key().id);
        let replacement_nonce = value.replacement_data.as_ref().map(|r| r.sequence_number);
        let signing_address = value.signer.or_else(|| {
            value
                .replacement_data
                .as_ref()
                .and_then(|r| r.key.key().signer)
        });
        Self {
            txs: value
                .txs
                .into_iter()
                .map(|tx| tx.envelope_encoded_no_blobs())
                .collect(),
            metadata: RawBundleMetadata {
                block_number: value.block.map(U64::from),
                reverting_tx_hashes: value.reverting_tx_hashes,
                dropping_tx_hashes: value.dropping_tx_hashes,
                replacement_uuid,
                uuid: replacement_uuid,
                signing_address,
                refund_identity: value.refund_identity,
                min_timestamp: value.min_timestamp,
                max_timestamp: value.max_timestamp,
                replacement_nonce,
                refund_percent: value.refund.as_ref().map(|br| br.percent),
                refund_recipient: value.refund.as_ref().map(|br| br.recipient),
                refund_tx_hashes: value.refund.as_ref().map(|br| vec![br.tx_hash]),
                delayed_refund: value.refund.as_ref().map(|br| br.delayed),
                version: Some(Self::encode_version(value.version)),
                bundle_hash: value.external_hash,
            },
        }
    }

    pub fn encode_version(version: BundleVersion) -> String {
        match version {
            BundleVersion::V1 => BUNDLE_VERSION_V1.to_string(),
            BundleVersion::V2 => BUNDLE_VERSION_V2.to_string(),
        }
    }

    fn into_recovered(self, txs: Vec<TransactionSignedEcRecoveredWithBlobs>) -> RawBundleRecovered {
        RawBundleRecovered {
            txs,
            metadata: self.metadata,
        }
    }
}

impl RawBundleRecovered {
    fn parse_refund(&mut self) -> Result<Option<BundleRefund>, RawBundleConvertError> {
        // Validate refund percent setting.
        if let Some(percent) = self.metadata.refund_percent {
            if percent >= 100 {
                return Err(RawBundleConvertError::InvalidRefundPercent(percent));
            }
            if percent == 0 {
                self.metadata.refund_percent = None
            }
        }

        let mut refund = None;
        if let Some(percent) = self.metadata.refund_percent {
            // Refund can be configured only if bundle is not empty.
            // If bundle contains only one transaction, first == last.
            // If refund_tx_hashes is empty we use the last tx.
            if let Some((first_tx, last_tx)) = self.txs.first().zip(self.txs.last()) {
                let tx_hash = if let Some(ref refund_tx_hashes) = self.metadata.refund_tx_hashes {
                    if refund_tx_hashes.len() > 1 {
                        return Err(RawBundleConvertError::MoreThanOneRefundTxHash);
                    }
                    refund_tx_hashes.first().copied()
                } else {
                    None
                }
                .unwrap_or(last_tx.hash());

                refund = Some(BundleRefund {
                    percent,
                    recipient: self
                        .metadata
                        .refund_recipient
                        .unwrap_or_else(|| first_tx.signer()),
                    tx_hash,
                    delayed: self.metadata.delayed_refund.unwrap_or_default(),
                });
            }
        }
        Ok(refund)
    }
}

/// Struct to de/serialize json Bundles from bundles APIs and from/db.
/// Does not assume a particular format on txs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawTx {
    pub tx: Bytes,
}

impl RawTx {
    pub fn decode(self, encoding: TxEncoding) -> Result<MempoolTx, TxWithBlobsCreateError> {
        Ok(MempoolTx::new(encoding.decode(self.tx)?))
    }

    /// See [TransactionSignedEcRecoveredWithBlobs::envelope_encoded_no_blobs]
    pub fn encode_no_blobs(value: MempoolTx) -> Self {
        Self {
            tx: value.tx_with_blobs.envelope_encoded_no_blobs(),
        }
    }
}

/// Struct to de/serialize json Bundles from bundles APIs and from/db.
/// Does not assume a particular format on txs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawShareBundle {
    pub version: String,
    pub inclusion: RawShareBundleInclusion,
    pub body: Vec<RawShareBundleBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validity: Option<RawShareBundleValidity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RawShareBundleMetadatada>,
    pub replacement_uuid: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawShareBundleInclusion {
    pub block: U64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_block: Option<U64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawShareBundleBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx: Option<Bytes>,
    #[serde(default)]
    pub can_revert: bool,
    pub revert_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<Box<RawShareBundle>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawShareBundleValidity {
    #[serde(default)]
    pub refund: Vec<Refund>,
    #[serde(default)]
    pub refund_config: Vec<RefundConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawShareBundleMetadatada {
    #[serde(default)]
    pub signer: Option<Address>,
    /// See [`ShareBundleReplacementData`] sequence_number
    pub replacement_nonce: Option<u64>,
    /// Used for cancelling. When true the only thing we care about is signer,replacement_nonce and RawShareBundle::replacement_uuid
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Error, Debug)]
pub enum RawShareBundleConvertError {
    #[error("Failed to decode transaction, idx: {0}, error: {1}")]
    FailedToDecodeTransaction(usize, TxWithBlobsCreateError),
    #[error("Bundle too deep")]
    BundleTooDeep,
    #[error("Incorrect version")]
    IncorrectVersion,
    #[error("Empty body")]
    EmptyBody,
    #[error("Total refund percent exceeds 100")]
    TotalRefundTooBig,
    #[error("Refund config does not add to 100")]
    RefundConfigIncorrect,
    #[error("Found cancel on decode_new_bundle")]
    FoundCancelExpectingBundle,
    #[error("Unable to parse a Cancel")]
    CancelError,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelShareBundle {
    pub block: u64,
    pub key: ShareBundleReplacementKey,
}
/// Since we use the same API (mev_sendBundle) to get new bundles and also to cancel them we need this struct
#[allow(clippy::large_enum_variant)]
pub enum RawShareBundleDecodeResult {
    NewShareBundle(Box<ShareBundle>),
    CancelShareBundle(CancelShareBundle),
}

impl RawShareBundle {
    /// Same as decode but fails on cancel
    pub fn decode_new_bundle(
        self,
        encoding: TxEncoding,
    ) -> Result<ShareBundle, RawShareBundleConvertError> {
        let decode_res = self.decode(encoding)?;
        match decode_res {
            RawShareBundleDecodeResult::NewShareBundle(b) => Ok(*b),
            RawShareBundleDecodeResult::CancelShareBundle(_) => {
                Err(RawShareBundleConvertError::FoundCancelExpectingBundle)
            }
        }
    }

    pub fn decode(
        self,
        encoding: TxEncoding,
    ) -> Result<RawShareBundleDecodeResult, RawShareBundleConvertError> {
        let (block, max_block) = (
            self.inclusion.block.to(),
            self.inclusion
                .max_block
                .unwrap_or(self.inclusion.block)
                .to(),
        );

        let signer = self.metadata.as_ref().and_then(|m| m.signer);
        let replacement_nonce = self.metadata.as_ref().and_then(|m| m.replacement_nonce);
        let replacement_data =
            if let (Some(replacement_uuid), Some(signer), Some(replacement_nonce)) =
                (self.replacement_uuid, signer, replacement_nonce)
            {
                Some(ShareBundleReplacementData {
                    key: ShareBundleReplacementKey::new(replacement_uuid, signer),
                    sequence_number: replacement_nonce,
                })
            } else {
                None
            };

        if self.metadata.as_ref().is_some_and(|r| r.cancelled) {
            return Ok(RawShareBundleDecodeResult::CancelShareBundle(
                CancelShareBundle {
                    block,
                    key: replacement_data
                        .ok_or(RawShareBundleConvertError::CancelError)?
                        .key,
                },
            ));
        }

        let (_, inner_bundle) = extract_inner_bundle(0, 0, self, &encoding)?;
        let bundle = ShareBundle::new(
            block,
            max_block,
            inner_bundle,
            signer,
            replacement_data,
            Vec::new(),
            Default::default(),
        );
        Ok(RawShareBundleDecodeResult::NewShareBundle(Box::new(bundle)))
    }

    /// See [TransactionSignedEcRecoveredWithBlobs::envelope_encoded_no_blobs]
    pub fn encode_no_blobs(value: ShareBundle) -> Self {
        let inclusion = RawShareBundleInclusion {
            block: U64::from(value.block),
            max_block: (value.block != value.max_block).then_some(U64::from(value.max_block)),
        };
        let mut result = inner_bundle_to_raw_bundle_no_blobs(inclusion, value.inner_bundle);
        result.metadata = value.signer.map(|signer| RawShareBundleMetadatada {
            signer: Some(signer),
            replacement_nonce: value.replacement_data.as_ref().map(|r| r.sequence_number),
            cancelled: false,
        });
        result.replacement_uuid = value.replacement_data.map(|r| r.key.0.id);
        result
    }
}

const TX_REVERT_NOT_ALLOWED: &str = "fail";
const TX_REVERT_ALLOWED_INCLUDED: &str = "allow";
const TX_REVERT_ALLOWED_EXCLUDED: &str = "drop";
fn serialize_revert_behavior(revert: TxRevertBehavior) -> String {
    match revert {
        TxRevertBehavior::NotAllowed => TX_REVERT_NOT_ALLOWED.to_owned(),
        TxRevertBehavior::AllowedIncluded => TX_REVERT_ALLOWED_INCLUDED.to_owned(),
        TxRevertBehavior::AllowedExcluded => TX_REVERT_ALLOWED_EXCLUDED.to_owned(),
    }
}

fn parse_revert_behavior(can_revert: bool, revert_mode: Option<String>) -> TxRevertBehavior {
    if let Some(revert_mode) = revert_mode {
        match revert_mode.as_str() {
            TX_REVERT_NOT_ALLOWED => TxRevertBehavior::NotAllowed,
            TX_REVERT_ALLOWED_INCLUDED => TxRevertBehavior::AllowedIncluded,
            TX_REVERT_ALLOWED_EXCLUDED => TxRevertBehavior::AllowedExcluded,
            _ => {
                error!(?revert_mode, "Illegal revert mode");
                TxRevertBehavior::NotAllowed
            }
        }
    } else {
        TxRevertBehavior::from_old_bool(can_revert)
    }
}

fn extract_inner_bundle(
    depth: usize,
    mut tx_count: usize,
    raw: RawShareBundle,
    encoding: &TxEncoding,
) -> Result<(usize, ShareBundleInner), RawShareBundleConvertError> {
    if depth > 5 {
        return Err(RawShareBundleConvertError::BundleTooDeep);
    }
    if raw.version != "v0.1" && raw.version != "version-1" && raw.version != "beta-1" {
        return Err(RawShareBundleConvertError::IncorrectVersion);
    }

    let body = raw
        .body
        .into_iter()
        .map(
            |body| -> Result<ShareBundleBody, RawShareBundleConvertError> {
                if let Some(tx) = body.tx {
                    let tx = encoding.decode(tx).map_err(|e| {
                        RawShareBundleConvertError::FailedToDecodeTransaction(tx_count, e)
                    })?;
                    tx_count += 1;
                    return Ok(ShareBundleBody::Tx(ShareBundleTx {
                        tx,
                        revert_behavior: parse_revert_behavior(body.can_revert, body.revert_mode),
                    }));
                }

                if let Some(bundle) = body.bundle {
                    // TODO: check that inclusion is correct

                    let (new_tx_count, extracted_inner_bundle) =
                        extract_inner_bundle(depth + 1, tx_count, *bundle, encoding)?;
                    tx_count = new_tx_count;
                    return Ok(ShareBundleBody::Bundle(extracted_inner_bundle));
                }

                Err(RawShareBundleConvertError::EmptyBody)
            },
        )
        .collect::<Result<Vec<_>, _>>()?;

    let (refund, refund_config) = raw
        .validity
        .map(|v| {
            if v.refund.iter().map(|r| r.percent).sum::<usize>() > 100 {
                return Err(RawShareBundleConvertError::TotalRefundTooBig);
            }

            if !v.refund_config.is_empty()
                && v.refund_config.iter().map(|r| r.percent).sum::<usize>() > 100
            {
                return Err(RawShareBundleConvertError::RefundConfigIncorrect);
            }

            Ok((v.refund, v.refund_config))
        })
        .unwrap_or_else(|| Ok((Vec::new(), Vec::new())))?;

    Ok((
        tx_count,
        ShareBundleInner {
            body,
            refund,
            refund_config,
            // mev-share does not allow this yet.
            can_skip: false,
            original_order_id: None,
        },
    ))
}

/// Txs serialized without blobs data (canonical format)
fn inner_bundle_to_raw_bundle_no_blobs(
    inclusion: RawShareBundleInclusion,
    inner: ShareBundleInner,
) -> RawShareBundle {
    let body = inner
        .body
        .into_iter()
        .map(|b| match b {
            ShareBundleBody::Bundle(inner) => RawShareBundleBody {
                tx: None,
                can_revert: false,
                revert_mode: None,
                bundle: Some(Box::new(inner_bundle_to_raw_bundle_no_blobs(
                    inclusion.clone(),
                    inner,
                ))),
            },
            ShareBundleBody::Tx(sbundle_tx) => {
                // We don't really need this since revert_mode takes priority over can_revert but just in case...
                let can_revert = sbundle_tx.revert_behavior.can_revert();
                let revert_mode = Some(serialize_revert_behavior(sbundle_tx.revert_behavior));
                RawShareBundleBody {
                    tx: Some(sbundle_tx.tx.envelope_encoded_no_blobs()),
                    can_revert,
                    revert_mode,
                    bundle: None,
                }
            }
        })
        .collect();

    let validity = (!inner.refund.is_empty() || !inner.refund_config.is_empty()).then_some(
        RawShareBundleValidity {
            refund: inner.refund,
            refund_config: inner.refund_config,
        },
    );

    RawShareBundle {
        version: String::from("v0.1"),
        inclusion,
        body,
        validity,
        metadata: None,
        replacement_uuid: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum RawOrder {
    Bundle(RawBundle),
    Tx(RawTx),
    ShareBundle(RawShareBundle),
}

#[derive(Error, Debug)]
pub enum RawOrderConvertError {
    #[error("Failed to decode bundle, error: {0}")]
    FailedToDecodeBundle(RawBundleConvertError),
    #[error("Failed to decode transaction, error: {0}")]
    FailedToDecodeTransaction(TxWithBlobsCreateError),
    #[error("Failed to decode share bundle`, error: {0}")]
    FailedToDecodeShareBundle(RawShareBundleConvertError),
    #[error("Blobs not supported by RawOrder")]
    BlobsNotSupported,
}

impl RawOrder {
    pub fn decode(self, encoding: TxEncoding) -> Result<Order, RawOrderConvertError> {
        match self {
            RawOrder::Bundle(bundle) => Ok(Order::Bundle(
                bundle
                    .decode_new_bundle(encoding)
                    .map_err(RawOrderConvertError::FailedToDecodeBundle)?,
            )),
            RawOrder::Tx(tx) => Ok(Order::Tx(
                tx.decode(encoding)
                    .map_err(RawOrderConvertError::FailedToDecodeTransaction)?,
            )),

            RawOrder::ShareBundle(bundle) => Ok(Order::ShareBundle(
                bundle
                    .decode_new_bundle(encoding)
                    .map_err(RawOrderConvertError::FailedToDecodeShareBundle)?,
            )),
        }
    }
}

impl From<Order> for RawOrder {
    fn from(value: Order) -> Self {
        match value {
            Order::Bundle(bundle) => Self::Bundle(RawBundle::encode_no_blobs(bundle)),
            Order::Tx(tx) => Self::Tx(RawTx::encode_no_blobs(tx)),
            Order::ShareBundle(bundle) => {
                Self::ShareBundle(RawShareBundle::encode_no_blobs(bundle))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplacementData;
    use alloy_consensus::Transaction;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{address, b256, bytes, fixed_bytes, keccak256, U256};
    use std::str::FromStr;
    use uuid::uuid;

    #[test]
    fn test_correct_bundle_decoding_v1() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v1",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": ["0xda7007bee134daa707d0e7399ce35bb451674f042fbbbcac3f6a3cb77846949c"],
            "minTimestamp": 0,
            "maxTimestamp": 1707136884,
            "signingAddress": "0x4696595f68034b47BbEc82dB62852B49a8EE7105"
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        let bundle_roundtrip = RawBundle::encode_no_blobs(bundle.clone());
        assert_eq!(bundle_request, bundle_roundtrip);

        assert_eq!(
            bundle.hash,
            fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
        );
        assert_eq!(bundle.uuid, uuid!("a90205bc-2afd-5afe-b315-f17d597ffd97"));

        assert_eq!(bundle.block, Some(18_050_847));
        assert_eq!(
            bundle.reverting_tx_hashes,
            vec![fixed_bytes!(
                "da7007bee134daa707d0e7399ce35bb451674f042fbbbcac3f6a3cb77846949c"
            )]
        );
        assert_eq!(bundle.txs.len(), 1);
        assert_eq!(bundle.refund, None);

        let tx = &bundle.txs[0].tx;
        assert_eq!(tx.nonce(), 973);
        assert_eq!(tx.gas_limit(), 231_610);
        assert_eq!(
            tx.to(),
            Some(address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD"))
        );
        assert_eq!(tx.value(), U256::from(0x80e531581b77c4u128));

        assert_eq!(bundle.min_timestamp, Some(0));
        assert_eq!(bundle.max_timestamp, Some(1_707_136_884));

        assert_eq!(
            bundle.signer,
            Some(address!("4696595f68034b47BbEc82dB62852B49a8EE7105"))
        );
    }

    #[test]
    fn test_correct_bundle_uuid_multiple_reverting_hashes_v1() {
        // reverting tx hashes ordering should not matter
        let inputs = [
            r#"
        {
            "version": "v1",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        }
        "#,
            r#"
        {
            "version": "v1",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"]
        }
        "#,
        ];

        for input in inputs {
            let bundle_request: RawBundle =
                serde_json::from_str(input).expect("failed to decode bundle");

            let bundle = bundle_request
                .decode_new_bundle(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to bundle");

            assert_eq!(
                bundle.hash,
                fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
            );
            assert_eq!(bundle.uuid, uuid!("d9a3ae52-79a2-5ce9-a687-e2aa4183d5c6"));
        }
    }

    #[test]
    fn test_correct_bundle_uuid_no_reverting_hashes_v1() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v1",
            "blockNumber": "0xA136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": []
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(
            bundle.hash,
            fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
        );
        assert_eq!(bundle.uuid, uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"));
    }

    #[test]
    fn test_correct_bundle_uuid_missing_reverting_hashes_v1() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v1",
            "blockNumber": "0xA136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"]
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(
            bundle.hash,
            fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
        );
        assert_eq!(bundle.uuid, uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"));
    }

    ///Real life case
    #[test]
    fn test_correct_bundle_uuid_null_reverting_hashes_v1() {
        // raw json string
        let bundle_json = r#"
        {
          "version": "v1",
          "txs": [
            "0x02f901c00182123184cd0a3c00850d8c3ac83483186a00949f51040aec194a89cb6a7e852e79ea07cc0bf6488203abb9014e524f05aadf99a0839818b3f120ebac9b73f82b617dc6a5550000000000000004aa7fdb4059a9fc0400000000000000000000000000000000000000000000000000000000000000000000000000540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e0000000000000001270250af8569d4ff712aaebc2f5971a824249fa7000000000000030015153da0e9e13cfc167b3d417d3721bf545479bb000bb800003c00540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e00000000000000015533b61d314f7faf87df530de362f457a342ec1e00000000000003008107fca5494375fc743a9fc4d4844353a1af3d94000bb800003c00540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e0000000000000001b81ab4b74522a25525e583f94dba73521cc4d56b0000000000000100308c6fbd6a14881af333649f17f2fde9cd75e2a6000000000000c080a061a306a26e0a66973364614912553f32c7915e899b188164bf2e99b97e08d0e8a00c76b844dc4b72c2040f14e69f0f9c3fa290a2db7c4a245d045155090ec7d746"
          ],
          "replacementUuid": null,
          "signingAddress": "0x564d55a3a73f6efb907afe92b1706602b2d54018",
          "blockNumber": "0x142dd19",
          "minTimestamp": null,
          "maxTimestamp": null,
          "revertingTxHashes": null
        }
        "#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        let bundle_roundtrip = RawBundle::encode_no_blobs(bundle.clone());
        assert_eq!(bundle_request, bundle_roundtrip);

        assert_eq!(
            bundle.hash,
            fixed_bytes!("08b57aa2df6e4729c55b809d1110f16aba30956cfc17f7ad771441d6d418f991")
        );
        assert_eq!(bundle.uuid, uuid!("0cc09d2b-6538-5d0e-a627-22c400845783"));

        assert!(bundle.reverting_tx_hashes.is_empty());
        assert_eq!(bundle.txs.len(), 1);

        assert_eq!(bundle.min_timestamp, None);
        assert_eq!(bundle.max_timestamp, None);
    }

    /// maxTimestamp should be considered None
    #[test]
    fn test_correct_bundle_zero_timestamp_decoding_v1() {
        // raw json string
        let bundle_json = r#"
        {
          "version": "v1",
          "txs": [
            "0x02f901c00182123184cd0a3c00850d8c3ac83483186a00949f51040aec194a89cb6a7e852e79ea07cc0bf6488203abb9014e524f05aadf99a0839818b3f120ebac9b73f82b617dc6a5550000000000000004aa7fdb4059a9fc0400000000000000000000000000000000000000000000000000000000000000000000000000540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e0000000000000001270250af8569d4ff712aaebc2f5971a824249fa7000000000000030015153da0e9e13cfc167b3d417d3721bf545479bb000bb800003c00540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e00000000000000015533b61d314f7faf87df530de362f457a342ec1e00000000000003008107fca5494375fc743a9fc4d4844353a1af3d94000bb800003c00540101d99034942c4a883ff3ed6cda6c91fe505a58eb2e0000000000000001b81ab4b74522a25525e583f94dba73521cc4d56b0000000000000100308c6fbd6a14881af333649f17f2fde9cd75e2a6000000000000c080a061a306a26e0a66973364614912553f32c7915e899b188164bf2e99b97e08d0e8a00c76b844dc4b72c2040f14e69f0f9c3fa290a2db7c4a245d045155090ec7d746"
          ],
          "minTimestamp": 0,
          "maxTimestamp": 0
        }
        "#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(
            bundle.hash,
            fixed_bytes!("08b57aa2df6e4729c55b809d1110f16aba30956cfc17f7ad771441d6d418f991")
        );
        assert_eq!(bundle.uuid, uuid!("3255ceb4-fdc5-592d-a501-2183727ca3df"));

        assert_eq!(bundle.min_timestamp, Some(0));
        assert_eq!(bundle.max_timestamp, None);
    }

    #[test]
    fn test_correct_bundle_uuid_multiple_dropping_hashes_v2() {
        // reverting tx hashes ordering should not matter
        let inputs = [
            r#"
        {
            "version": "v2",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "droppingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        }
        "#,
            r#"
        {
            "version": "v2",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"]
        }
        "#,
        ];

        for input in inputs {
            let bundle_request: RawBundle =
                serde_json::from_str(input).expect("failed to decode bundle");

            let bundle = bundle_request
                .decode_new_bundle(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to bundle");

            assert_eq!(
                bundle.hash,
                fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
            );
            assert_eq!(bundle.uuid, uuid!("7addcd74-5d07-5d05-8750-3f0858e09195"));
        }
    }

    /// blockNumber should considered None
    #[test]
    fn test_correct_bundle_decoding_refunds_no_block_v2() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v2",
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "minTimestamp": 123,
            "maxTimestamp": 1234,
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
            "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "refundPercent": 1,
            "refundRecipient": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5",
            "refundTxHashes": ["0x75662ab9cb6d1be7334723db5587435616352c7e581a52867959ac24006ac1fe"]
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");
        println!("{}", bundle.txs[0].hash());
        assert_eq!(bundle.block, None);
        assert_eq!(
            bundle.refund,
            Some(BundleRefund {
                percent: 1,
                recipient: Address::from_str("0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5").unwrap(),
                tx_hash: b256!(
                    "0x75662ab9cb6d1be7334723db5587435616352c7e581a52867959ac24006ac1fe"
                ),
                delayed: false,
            })
        );
        assert_eq!(bundle.uuid, uuid!("e2bdb8cd-9473-5a1b-b425-57fa7ecfe2c1"));
    }

    /// If refundTxHashes is missing it should use the last tx and the id should be the same.
    #[test]
    fn test_correct_bundle_decoding_refund_hash_missing() {
        // raw json string
        let base_bundle_json = r#"
        {
            "version": "v2",
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "minTimestamp": 123,
            "maxTimestamp": 1234,
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
            "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "refundPercent": 1,
            "refundRecipient": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5"
        "#;
        let bundles = [
            base_bundle_json.to_owned() + "}",
            base_bundle_json.to_owned()
                + r#","refundTxHashes": ["0x84310f7f7860f0cd65407fe340d471ca008d0c58976746a560312d4aebba3f4a"]}"#,
        ];

        for bundle_json in bundles {
            let bundle_request: RawBundle =
                serde_json::from_str(&bundle_json).expect("failed to decode bundle");

            let bundle = bundle_request
                .clone()
                .decode_new_bundle(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to bundle");
            assert_eq!(bundle.block, None);
            assert_eq!(
                bundle.refund,
                Some(BundleRefund {
                    percent: 1,
                    recipient: Address::from_str("0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5")
                        .unwrap(),
                    tx_hash: b256!(
                        "0x84310f7f7860f0cd65407fe340d471ca008d0c58976746a560312d4aebba3f4a"
                    ),
                    delayed: false,
                })
            );
            assert_eq!(bundle.uuid, uuid!("ea9954e1-b7be-5af0-9c39-6b11c9d24c05"));
        }
    }

    /// More than 1 refundTxHashes should fail.
    #[test]
    fn test_fail_bundle_decoding_2_refund_hashes() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v2",
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "minTimestamp": 123,
            "maxTimestamp": 1234,
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
            "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "refundPercent": 1,
            "refundRecipient": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5",
            "refundTxHashes": ["0x84310f7f7860f0cd65407fe340d471ca008d0c58976746a560312d4aebba3f4a","0x84310f7f7860f0cd65407fe340d471ca008d0c58976746a560312d4aebba3f4a"]
        }
        "#;
        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        assert!(matches!(
            bundle_request
                .clone()
                .decode_new_bundle(TxEncoding::WithBlobData),
            Err(RawBundleConvertError::MoreThanOneRefundTxHash)
        ));
    }

    /// Should default to last version.
    #[test]
    fn test_correct_bundle_decoding_no_version() {
        // raw json string
        let bundle_json = r#"
        {
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "minTimestamp": 123,
            "maxTimestamp": 1234
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(bundle.version, LAST_BUNDLE_VERSION);
        assert_eq!(bundle.refund, None);
    }

    /// empty txs,missing txs field or null should generate a cancellation.
    #[test]
    fn test_correct_bundle_cancellation_decoding() {
        let txs_fields = ["\"txs\": [],", "\"txs\": null,", ""];
        for txs_field in txs_fields {
            // raw json string
            let mut bundle_json = "{ ".to_string();
            bundle_json += txs_field;
            bundle_json += r#"
                        "blockNumber": 0,
                        "minTimestamp": 123,
                        "maxTimestamp": 1234,
                        "replacementUuid": "3255ceb4-fdc5-592d-a501-2183727ca3df",
                        "replacementNonce": 49,
                        "signingAddress": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5"
                    }"#;

            let bundle_request: RawBundle =
                serde_json::from_str(&bundle_json).expect("failed to decode bundle");

            let bundle = bundle_request
                .clone()
                .decode(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to RawBundleDecodeResult");
            if let RawBundleDecodeResult::CancelBundle(cancel) = bundle {
                assert_eq!(
                    cancel.key.key().id,
                    uuid!("3255ceb4-fdc5-592d-a501-2183727ca3df")
                );
                assert_eq!(
                    cancel.key.key().signer.unwrap(),
                    address!("0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5")
                );
                assert_eq!(cancel.sequence_number, 49);
            } else {
                panic!("Cancel RawBundle wrongly decoded");
            }
        }
    }

    #[test]
    /// droppingTxHashes/refundPercent/refundRecipient/refundTxHashes should fail to decode as v1
    fn test_error_bundle_decoding_invalid_fields_v1() {
        let base_bundle_json = r#"
        {
            "version": "v1",
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "minTimestamp": 123,
            "maxTimestamp": 1234,
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
        "#.to_owned();

        let extra_invalid_fields = [
            r#" "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] "#,
            r#" "refundPercent": 1 "#,
            r#" "refundRecipient": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5" "#,
            r#" "refundTxHashes": ["0x75662ab9cb6d1be7334723db5587435616352c7e581a52867959ac24006ac1fe"] "#,
        ];

        for field in extra_invalid_fields {
            let bundle_json = base_bundle_json.clone() + field + "}";
            println!("{bundle_json}");
            let bundle_request: RawBundle =
                serde_json::from_str(&bundle_json).expect("failed to decode bundle");
            let res = bundle_request
                .clone()
                .decode_new_bundle(TxEncoding::WithBlobData);
            assert!(matches!(
                res,
                Err(RawBundleConvertError::FieldNotSupportedByVersion(
                    _,
                    BundleVersion::V1
                ))
            ));
        }
    }

    #[test]
    fn test_correct_bundle_decoding_no_timestamps_v2() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v2",
            "txs": [
                "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
            ],
            "blockNumber": 0,
            "revertingTxHashes": []
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(bundle.min_timestamp, None);
        assert_eq!(bundle.max_timestamp, None);
        assert_eq!(bundle.uuid, uuid!("22dc6bf0-9a12-5a76-9bbd-98ab77423415"));
    }

    /// This tests only the generated uuid.
    #[test]
    fn test_bundle_uuid() {
        struct Test {
            rpc_json: &'static str,
            expected_uuid: Uuid,
        }
        let tests = [
            Test {
                rpc_json: r#"
            {
				"version": "v1",
				"blockNumber": "0x1136F1F",
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        	}
            "#,
                expected_uuid: uuid!("d9a3ae52-79a2-5ce9-a687-e2aa4183d5c6"),
            },
            Test {
                rpc_json: r#"
            {
				"version": "v1",
				"blockNumber": "0x1136F1F",
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"]
			}
                "#,
                expected_uuid: uuid!("d9a3ae52-79a2-5ce9-a687-e2aa4183d5c6"),
            },
            Test {
                rpc_json: r#"
            {
				"version": "v1",
				"blockNumber": "0xA136F1F",
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": []
			}
                "#,
                expected_uuid: uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"),
            },
            Test {
                rpc_json: r#"
            {
				"version": "v1",
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": []
			}
                "#,
                expected_uuid: uuid!("e9ced844-16d5-5884-8507-db9338950c5c"),
            },
            Test {
                rpc_json: r#"
            {
				"version": "v1",
		        "blockNumber": "0x0",
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": []
			}
                "#,
                expected_uuid: uuid!("e9ced844-16d5-5884-8507-db9338950c5c"),
            },
            Test {
                rpc_json: r#"
            {
				"version": "v1",
		        "blockNumber": null,
				"txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
				"revertingTxHashes": []
			}
                "#,
                expected_uuid: uuid!("e9ced844-16d5-5884-8507-db9338950c5c"),
            },
            Test {
                rpc_json: r#"
            {
                "version": "v2",
                "txs": [
                    "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
                ],
                "blockNumber": "0x0",
                "minTimestamp": 123,
                "maxTimestamp": 1234,
                "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
                "droppingTxHashes": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
                "refundPercent": 1,
                "refundRecipient": "0x95222290dd7278aa3ddd389cc1e1d165cc4bafe5",
                "refundTxHashes": ["0x75662ab9cb6d1be7334723db5587435616352c7e581a52867959ac24006ac1fe"]
            }
                "#,
                expected_uuid: uuid!("e2bdb8cd-9473-5a1b-b425-57fa7ecfe2c1"),
            },
            Test {
                rpc_json: r#"
            {
                "version": "v2",
                "txs": [
                    "0x02f90408018303f1d4808483ab318e8304485c94a69babef1ca67a37ffaf7a485dfff3382056e78c8302be00b9014478e111f60000000000000000000000007f0f35bbf44c8343d14260372c469b331491567b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000c4f4ff52950000000000000000000000000000000000000000000000000be75df44ebec5390000000000000000000000000000000000000000000036404c073ad050000000000000000000000000000000000000000000003e91fd871e8a6021ca93d911920000000000000000000000000000000000000000000000000000e91615b961030000000000000000000000000000000000000000000000000000000067eaa0b7ff8000000000000000000000000000000000000000000000000000000001229300000000000000000000000000000000000000000000000000000000f90253f9018394919fa96e88d67499339577fa202345436bcdaf79f9016ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513782a000000000000000000000000000000000000000000000000000000000000000a1a0bfd358e93f18da3ed276c3afdbdba00b8f0b6008a03476a6a86bd6320ee6938ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513785a00000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000a0a00000000000000000000000000000000000000000000000000000000000000002a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513783a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513784a00000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000004f85994c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2f842a060802b93a9ac49b8c74d6ade12cf6235f4ac8c52c84fd39da757d0b2d720d76fa075245230289a9f0bf73a6c59aef6651b98b3833a62a3c0bd9ab6b0dec8ed4d8fd6947f0f35bbf44c8343d14260372c469b331491567bc0f85994d533a949740bb3306d119cc777fa900ba034cd52f842a07a7ff188ddb962db42160fb3fb573f4af0ebe1a1d6b701f1f1464b5ea43f7638a03d4653d86fe510221a71cfd2b1168b2e9af3e71339c63be5f905dabce97ee61f01a0c9d68ec80949077b6c28d45a6bf92727bc49d705d201bff8c62956201f5d3a81a036b7b953d7385d8fab8834722b7c66eea4a02a66434fc4f38ebfe8f5218a87b0"
                ],
                "blockNumber": "0x0",
                "minTimestamp": 123,
                "maxTimestamp": 1234,
                "refundPercent": 20,
                "refundRecipient": "0xFF82BF5238637B7E5E345888BaB9cd99F5Ebe331",
                "refundTxHashes": ["0xffd9f02004350c16b312fd14ccc828f587c3c49ad3e9293391a398cc98c1a373"]
            }
                "#,
                expected_uuid: uuid!("e785c7c0-8bfa-508e-9c3f-cb24f1638de3"),
            },
            Test {
                rpc_json: r#"
            {
                "version": "v2",
                "txs": [
                    "0x02f90408018303f1d4808483ab318e8304485c94a69babef1ca67a37ffaf7a485dfff3382056e78c8302be00b9014478e111f60000000000000000000000007f0f35bbf44c8343d14260372c469b331491567b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000c4f4ff52950000000000000000000000000000000000000000000000000be75df44ebec5390000000000000000000000000000000000000000000036404c073ad050000000000000000000000000000000000000000000003e91fd871e8a6021ca93d911920000000000000000000000000000000000000000000000000000e91615b961030000000000000000000000000000000000000000000000000000000067eaa0b7ff8000000000000000000000000000000000000000000000000000000001229300000000000000000000000000000000000000000000000000000000f90253f9018394919fa96e88d67499339577fa202345436bcdaf79f9016ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513782a000000000000000000000000000000000000000000000000000000000000000a1a0bfd358e93f18da3ed276c3afdbdba00b8f0b6008a03476a6a86bd6320ee6938ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513785a00000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000a0a00000000000000000000000000000000000000000000000000000000000000002a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513783a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513784a00000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000004f85994c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2f842a060802b93a9ac49b8c74d6ade12cf6235f4ac8c52c84fd39da757d0b2d720d76fa075245230289a9f0bf73a6c59aef6651b98b3833a62a3c0bd9ab6b0dec8ed4d8fd6947f0f35bbf44c8343d14260372c469b331491567bc0f85994d533a949740bb3306d119cc777fa900ba034cd52f842a07a7ff188ddb962db42160fb3fb573f4af0ebe1a1d6b701f1f1464b5ea43f7638a03d4653d86fe510221a71cfd2b1168b2e9af3e71339c63be5f905dabce97ee61f01a0c9d68ec80949077b6c28d45a6bf92727bc49d705d201bff8c62956201f5d3a81a036b7b953d7385d8fab8834722b7c66eea4a02a66434fc4f38ebfe8f5218a87b0"
                ],
                "blockNumber": "0x0",
                "minTimestamp": 123,
                "maxTimestamp": 1234,
                "refundPercent": 20,
                "refundRecipient": "0xFF82BF5238637B7E5E345888BaB9cd99F5Ebe331"
            }
                "#,
                expected_uuid: uuid!("e785c7c0-8bfa-508e-9c3f-cb24f1638de3"),
            },
            Test {
                rpc_json: r#"
            {
                "version": "v2",
                "txs": [
                    "0x02f90408018303f1d4808483ab318e8304485c94a69babef1ca67a37ffaf7a485dfff3382056e78c8302be00b9014478e111f60000000000000000000000007f0f35bbf44c8343d14260372c469b331491567b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000c4f4ff52950000000000000000000000000000000000000000000000000be75df44ebec5390000000000000000000000000000000000000000000036404c073ad050000000000000000000000000000000000000000000003e91fd871e8a6021ca93d911920000000000000000000000000000000000000000000000000000e91615b961030000000000000000000000000000000000000000000000000000000067eaa0b7ff8000000000000000000000000000000000000000000000000000000001229300000000000000000000000000000000000000000000000000000000f90253f9018394919fa96e88d67499339577fa202345436bcdaf79f9016ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513782a000000000000000000000000000000000000000000000000000000000000000a1a0bfd358e93f18da3ed276c3afdbdba00b8f0b6008a03476a6a86bd6320ee6938ba0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513785a00000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000a0a00000000000000000000000000000000000000000000000000000000000000002a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513783a0bf1c200cc6dee22da7e010c51ff8e5210da52f1c78d2171dbb5d4f739e513784a00000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000004f85994c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2f842a060802b93a9ac49b8c74d6ade12cf6235f4ac8c52c84fd39da757d0b2d720d76fa075245230289a9f0bf73a6c59aef6651b98b3833a62a3c0bd9ab6b0dec8ed4d8fd6947f0f35bbf44c8343d14260372c469b331491567bc0f85994d533a949740bb3306d119cc777fa900ba034cd52f842a07a7ff188ddb962db42160fb3fb573f4af0ebe1a1d6b701f1f1464b5ea43f7638a03d4653d86fe510221a71cfd2b1168b2e9af3e71339c63be5f905dabce97ee61f01a0c9d68ec80949077b6c28d45a6bf92727bc49d705d201bff8c62956201f5d3a81a036b7b953d7385d8fab8834722b7c66eea4a02a66434fc4f38ebfe8f5218a87b0"
                ],
                "blockNumber": "0x0",
                "minTimestamp": 123,
                "maxTimestamp": 1234,
                "refundPercent": 20
            }
                "#,
                expected_uuid: uuid!("e785c7c0-8bfa-508e-9c3f-cb24f1638de3"),
            },
            Test {
                rpc_json: r#"
            {
                "version": "v2",
                "txs": [
                    "0x02f86b83aa36a780800982520894f24a01ae29dec4629dfb4170647c4ed4efc392cd861ca62a4c95b880c080a07d37bb5a4da153a6fbe24cf1f346ef35748003d1d0fc59cf6c17fb22d49e42cea02c231ac233220b494b1ad501c440c8b1a34535cdb8ca633992d6f35b14428672"
                ],
                "blockNumber": "0x0",
                "revertingTxHashes": []
            }
                "#,
                expected_uuid: uuid!("22dc6bf0-9a12-5a76-9bbd-98ab77423415"),
            },
        ];
        for test in tests {
            println!("{}", test.rpc_json);
            let bundle_request: RawBundle =
                serde_json::from_str(test.rpc_json).expect("failed to decode bundle");
            let bundle = bundle_request
                .clone()
                .decode_new_bundle(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to bundle");
            assert_eq!(bundle.uuid, test.expected_uuid);
        }
    }

    #[test]
    fn test_correct_raw_tx_decoding() {
        // raw json string
        let tx_json = r#"
        {
            "tx": "0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"
        }"#;

        let raw_tx_request: RawTx = serde_json::from_str(tx_json).expect("failed to decode tx");

        let tx = raw_tx_request
            .clone()
            .decode(TxEncoding::WithBlobData)
            .expect("failed to convert raw request to tx")
            .tx_with_blobs
            .tx;

        let raw_tx_roundtrip = RawTx {
            tx: {
                let mut buf = Vec::new();
                tx.encode_2718(&mut buf);
                buf.into()
            },
        };
        assert_eq!(raw_tx_request, raw_tx_roundtrip);

        assert_eq!(tx.nonce(), 973);
        assert_eq!(tx.gas_limit(), 231_610);
        assert_eq!(
            tx.to(),
            Some(address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD"))
        );
        assert_eq!(tx.value(), U256::from(36280797113317316u128));
    }

    #[test]
    fn test_correct_share_bundle_decoding() {
        // raw json string
        let bundle_json = r#"
        {
            "version": "v0.1",
            "inclusion": {
              "block": "0x1",
              "maxBlock": "0x11"
            },
            "body": [
              {
                "bundle": {
                  "version": "v0.1",
                  "inclusion": {
                    "block": "0x1"
                  },
                  "body": [
                    {
                      "tx": "0x02f86b0180843b9aca00852ecc889a0082520894c87037874aed04e51c29f582394217a0a2b89d808080c080a0a463985c616dd8ee17d7ef9112af4e6e06a27b071525b42182fe7b0b5c8b4925a00af5ca177ffef2ff28449292505d41be578bebb77110dfc09361d2fb56998260",
                      "canRevert": true
                    },
                    {
                      "tx": "0x02f8730180843b9aca00852ecc889a008288b894c10000000000000000000000000000000000000088016345785d8a000080c001a07c8890151fed9a826f241d5a37c84062ebc55ca7f5caef4683dcda6ac99dbffba069108de72e4051a764f69c51a6b718afeff4299107963a5d84d5207b2d6932a4",
                      "revertMode": "drop"
                    }
                  ],
                  "validity": {
                    "refund": [
                      {
                        "bodyIdx": 0,
                        "percent": 90
                      }
                    ],
                    "refundConfig": [
                      {
                        "address": "0x3e7dfb3e26a16e3dbf6dfeeff8a5ae7a04f73aad",
                        "percent": 100
                      }
                    ]
                  }
                }
              },
              {
                "tx": "0x02f8730101843b9aca00852ecc889a008288b894c10000000000000000000000000000000000000088016345785d8a000080c001a0650c394d77981e46be3d8cf766ecc435ec3706375baed06eb9bef21f9da2828da064965fdf88b91575cd74f20301649c9d011b234cefb6c1761cc5dd579e4750b1"
              }
            ],
            "validity": {
              "refund": [
                {
                  "bodyIdx": 0,
                  "percent": 80
                }
              ]
            },
            "metadata": {
                "signer": "0x4696595f68034b47BbEc82dB62852B49a8EE7105",
                "replacementNonce": 17
            },
            "replacementUuid": "3255ceb4-fdc5-592d-a501-2183727ca3df"
        }
        "#;

        let bundle_request: RawShareBundle =
            serde_json::from_str(bundle_json).expect("failed to decode share bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert share bundle request to share bundle");
        let bundle_clone = bundle.clone();

        assert_eq!(bundle.block, 0x1);
        assert_eq!(bundle.max_block, 0x11);
        assert_eq!(
            bundle
                .flatten_txs()
                .into_iter()
                .map(|(tx, opt)| (tx.hash(), opt))
                .collect::<Vec<_>>(),
            vec![
                (
                    fixed_bytes!(
                        "ec5dd7d793a20885a822169df4030d92fbc8d3ac5bd9eaa190b82196ea2858da"
                    ),
                    true
                ),
                (
                    fixed_bytes!(
                        "ba8dd77f4e9cf3c833399dc7f25408bb35fee78787a039e0ce3c80b04c537a71"
                    ),
                    true
                ),
                (
                    fixed_bytes!(
                        "e8953f516797ef26566c705be13c7cc77dd0f557c734b8278fac091f13b0d46a"
                    ),
                    false
                ),
            ]
        );

        let expected_hash = keccak256(
            [
                keccak256(
                    [
                        fixed_bytes!(
                            "ec5dd7d793a20885a822169df4030d92fbc8d3ac5bd9eaa190b82196ea2858da"
                        )
                        .to_vec(),
                        fixed_bytes!(
                            "ba8dd77f4e9cf3c833399dc7f25408bb35fee78787a039e0ce3c80b04c537a71"
                        )
                        .to_vec(),
                    ]
                    .concat(),
                )
                .to_vec(),
                fixed_bytes!("e8953f516797ef26566c705be13c7cc77dd0f557c734b8278fac091f13b0d46a")
                    .to_vec(),
            ]
            .concat(),
        );
        assert_eq!(bundle.hash, expected_hash);
        assert_eq!(
            bundle.signer,
            Some(address!("4696595f68034b47BbEc82dB62852B49a8EE7105"))
        );

        let b = bundle.inner_bundle;
        assert_eq!(b.body.len(), 2);
        assert!(matches!(b.body[0], ShareBundleBody::Bundle(..)));
        assert!(matches!(
            b.body[1],
            ShareBundleBody::Tx(ShareBundleTx {
                revert_behavior: TxRevertBehavior::NotAllowed,
                ..
            })
        ));
        assert_eq!(
            b.refund,
            vec![Refund {
                body_idx: 0,
                percent: 80
            }]
        );
        assert!(b.refund_config.is_empty());

        let b = if let ShareBundleBody::Bundle(b) = &b.body[0] {
            b.clone()
        } else {
            unreachable!()
        };
        assert_eq!(b.body.len(), 2);
        assert!(matches!(
            b.body[0],
            ShareBundleBody::Tx(ShareBundleTx {
                revert_behavior: TxRevertBehavior::AllowedIncluded,
                ..
            })
        ));
        assert!(matches!(
            b.body[1],
            ShareBundleBody::Tx(ShareBundleTx {
                revert_behavior: TxRevertBehavior::AllowedExcluded,
                ..
            })
        ));
        assert_eq!(
            b.refund,
            vec![Refund {
                body_idx: 0,
                percent: 90
            }]
        );
        assert_eq!(
            b.refund_config,
            vec![RefundConfig {
                address: address!("3e7dfb3e26a16e3dbf6dfeeff8a5ae7a04f73aad"),
                percent: 100
            }]
        );

        assert_eq!(
            bundle.replacement_data,
            Some(ReplacementData {
                key: ShareBundleReplacementKey::new(
                    uuid!("3255ceb4-fdc5-592d-a501-2183727ca3df"),
                    address!("4696595f68034b47BbEc82dB62852B49a8EE7105")
                ),
                sequence_number: 17,
            })
        );

        // There are differences in json when decoding and encdoding bundle but we want our internal structures to match
        let bundle_roundtrip = RawShareBundle::encode_no_blobs(bundle_clone.clone());
        let bundle_roundtrip = bundle_roundtrip
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert roundrrip share bundle request to share bundle");
        assert_eq!(bundle_clone, bundle_roundtrip);
    }

    #[test]
    fn test_correct_raw_order_decoding() {
        // raw json string
        let bundle_json = r#"
        {
            "type": "bundle",
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": ["0xda7007bee134daa707d0e7399ce35bb451674f042fbbbcac3f6a3cb77846949c"]
        }"#;

        let raw_order: RawOrder =
            serde_json::from_str(bundle_json).expect("failed to decode raw order with bundle");
        assert!(matches!(raw_order, RawOrder::Bundle(_)));

        let raw_tx_json = r#"{
            "type": "tx",
            "tx": "0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"
        }"#;

        let raw_order: RawOrder =
            serde_json::from_str(raw_tx_json).expect("failed to decode raw order with tx");
        assert!(matches!(raw_order, RawOrder::Tx(_)));
    }

    /// We decode a 4484 Tx in canonical format using WithBlobData which is for network format.
    /// We expect the specific error FailedToDecodeTransactionProbablyIs4484Canonical.
    #[test]
    fn test_correct_mixed_blob_mode_decoding() {
        let raw_tx =  bytes!("03f9021b01829f1084db518e44850efef5c902830249f09406a9ab27c7e2255df1815e6cc0168d7755feb19a80b90184648885fb000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000001600000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000066cc9a0eb519e9e1de68f6cf0aa1aa1efe3723d50000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001efcf00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0843b9aca00e1a00154404fdaef0e93e6a4df6aa099f66fc4f90267b3ef5bb6ac4d3f77a456ae5180a01367ff3e4598620be9424d5be0deafe5b3d3b7221c5f5c3d9fade0f545b19890a0026cd9941cd2aa4df41d5d36aa2e82a671c3226f2924cb206363a9458f38b8f6");
        let raw_tx_order = RawTx { tx: raw_tx };
        let tx_res = raw_tx_order.decode(TxEncoding::WithBlobData);
        assert!(matches!(
            tx_res,
            Err(TxWithBlobsCreateError::FailedToDecodeTransactionProbablyIs4484Canonical(_))
        ));
    }
}
