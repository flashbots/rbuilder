use super::{
    Bundle, BundleReplacementData, BundleReplacementKey, MempoolTx, Order,
    RawTxWithBlobsConvertError, Refund, RefundConfig, ShareBundle, ShareBundleBody,
    ShareBundleInner, ShareBundleReplacementData, ShareBundleReplacementKey, ShareBundleTx,
    TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior,
};
use alloy_primitives::Address;
use reth_primitives::{Bytes, B256, U64};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DefaultOnNull};
use thiserror::Error;
use tracing::error;
use uuid::Uuid;

/// Encoding mode for raw transactions (https://eips.ethereum.org/EIPS/eip-4844)
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
    ) -> Result<TransactionSignedEcRecoveredWithBlobs, RawTxWithBlobsConvertError> {
        match self {
            TxEncoding::NoBlobData => {
                TransactionSignedEcRecoveredWithBlobs::decode_enveloped_with_fake_blobs(raw_tx)
            }
            TxEncoding::WithBlobData => {
                TransactionSignedEcRecoveredWithBlobs::decode_enveloped_with_real_blobs(raw_tx)
            }
        }
    }
}

/// Struct to de/serialize json Bundles from bundles APIs and from/db.
/// Does not assume a particular format on txs.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBundle {
    pub block_number: U64,
    pub txs: Vec<Bytes>,
    #[serde_as(deserialize_as = "DefaultOnNull")]
    pub reverting_tx_hashes: Vec<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_address: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_timestamp: Option<u64>,
    /// See [`BundleReplacementData`] sequence_number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_nonce: Option<u64>,
}

#[derive(Error, Debug)]
pub enum RawBundleConvertError {
    #[error("Failed to decode transaction, idx: {0}, error: {0}")]
    FailedToDecodeTransaction(usize, RawTxWithBlobsConvertError),
    #[error("Incorrect replacement data")]
    IncorrectReplacementData,
    #[error("Blobs not supported by RawBundle")]
    BlobsNotSupported,
}

impl RawBundle {
    pub fn decode(self, encoding: TxEncoding) -> Result<Bundle, RawBundleConvertError> {
        let txs = self
            .txs
            .into_iter()
            .enumerate()
            .map(|(idx, tx)| {
                encoding
                    .decode(tx)
                    .map_err(|e| RawBundleConvertError::FailedToDecodeTransaction(idx, e))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let replacement_data = Self::decode_replacement_data(
            self.replacement_uuid,
            self.signing_address,
            self.replacement_nonce,
        )?;
        let reverting_tx_hashes = {
            let mut sorted_reverting_hashes = self.reverting_tx_hashes;
            sorted_reverting_hashes.sort();
            sorted_reverting_hashes
        };

        let mut bundle = Bundle {
            block: self.block_number.to(),
            txs,
            reverting_tx_hashes,
            hash: Default::default(),
            uuid: Default::default(),
            replacement_data,
            min_timestamp: self.min_timestamp,
            max_timestamp: self.max_timestamp,
            signer: self.signing_address,
            metadata: Default::default(),
        };
        bundle.hash_slow();
        Ok(bundle)
    }

    /// consistency checks on raw data.
    fn decode_replacement_data(
        replacement_uuid: Option<Uuid>,
        signing_address: Option<Address>,
        replacement_nonce: Option<u64>,
    ) -> Result<Option<BundleReplacementData>, RawBundleConvertError> {
        let got_uuid = replacement_uuid.is_some();
        let got_nonce = replacement_nonce.is_some();
        if got_uuid != got_nonce {
            return Err(RawBundleConvertError::IncorrectReplacementData);
        }
        if !got_uuid {
            return Ok(None);
        }
        if let (Some(uuid), Some(signer), Some(sequence_number)) =
            (replacement_uuid, signing_address, replacement_nonce)
        {
            Ok(Some(BundleReplacementData {
                key: BundleReplacementKey::new(uuid, signer),
                sequence_number,
            }))
        } else {
            Err(RawBundleConvertError::IncorrectReplacementData) //got_uuid && got_nonce && !signer
        }
    }

    /// See [TransactionSignedEcRecoveredWithBlobs::envelope_encoded_no_blobs]
    pub fn encode_no_blobs(value: Bundle) -> Self {
        let replacement_uuid = value.replacement_data.as_ref().map(|r| r.key.key().id);
        let replacement_nonce = value.replacement_data.as_ref().map(|r| r.sequence_number);
        let signing_address = value
            .signer
            .or(value.replacement_data.map(|r| r.key.key().signer));
        Self {
            block_number: U64::from(value.block),
            txs: value
                .txs
                .into_iter()
                .map(|tx| tx.envelope_encoded_no_blobs())
                .collect(),
            reverting_tx_hashes: value.reverting_tx_hashes,
            replacement_uuid,
            signing_address,
            min_timestamp: value.min_timestamp,
            max_timestamp: value.max_timestamp,
            replacement_nonce,
        }
    }
}

/// Struct to de/serialize json Bundles from bundles APIs and from/db.
/// Does not assume a particular format on txs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawTx {
    pub tx: Bytes,
}

impl RawTx {
    pub fn decode(self, encoding: TxEncoding) -> Result<MempoolTx, RawTxWithBlobsConvertError> {
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
    #[error("Failed to decode transaction, idx: {0}, error: {0}")]
    FailedToDecodeTransaction(usize, RawTxWithBlobsConvertError),
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

#[derive(Clone, Debug)]
pub struct CancelShareBundle {
    pub block: u64,
    pub key: ShareBundleReplacementKey,
}
/// Since we use the same API (mev_sendBundle) to get new bundles and also to cancel them we need this struct
pub enum RawShareBundleDecodeResult {
    NewShareBundle(ShareBundle),
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
            RawShareBundleDecodeResult::NewShareBundle(b) => Ok(b),
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

        if self.metadata.as_ref().map_or(false, |r| r.cancelled) {
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
        let mut bundle = ShareBundle {
            hash: Default::default(),
            block,
            max_block,
            inner_bundle,
            signer,
            replacement_data,
            original_orders: Vec::new(),
            metadata: Default::default(),
        };

        bundle.hash_slow();

        Ok(RawShareBundleDecodeResult::NewShareBundle(bundle))
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
            replacement_nonce: None,
            cancelled: false,
        });
        result
    }
}

fn create_revert_behavior(can_revert: bool, revert_mode: Option<String>) -> TxRevertBehavior {
    if let Some(revert_mode) = revert_mode {
        match revert_mode.as_str() {
            "fail" => TxRevertBehavior::NotAllowed,
            "allow" => TxRevertBehavior::AllowedIncluded,
            "drop" => TxRevertBehavior::AllowedExcluded,
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
    if raw.version != "v0.1" && raw.version != "version-1" {
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
                        revert_behavior: create_revert_behavior(body.can_revert, body.revert_mode),
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
            ShareBundleBody::Tx(sbundle_tx) => RawShareBundleBody {
                tx: Some(sbundle_tx.tx.envelope_encoded_no_blobs()),
                can_revert: sbundle_tx.revert_behavior.can_revert(),
                revert_mode: None,
                bundle: None,
            },
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    FailedToDecodeTransaction(RawTxWithBlobsConvertError),
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
                    .decode(encoding)
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
    use alloy_primitives::{address, fixed_bytes, keccak256, U256};
    use uuid::uuid;

    #[test]
    fn test_correct_bundle_decoding() {
        // raw json string
        let bundle_json = r#"
        {
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
            .decode(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        let bundle_roundtrip = RawBundle::encode_no_blobs(bundle.clone());
        assert_eq!(bundle_request, bundle_roundtrip);

        assert_eq!(
            bundle.hash,
            fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
        );
        assert_eq!(bundle.uuid, uuid!("a90205bc-2afd-5afe-b315-f17d597ffd97"));

        assert_eq!(bundle.block, 18_050_847);
        assert_eq!(
            bundle.reverting_tx_hashes,
            vec![fixed_bytes!(
                "da7007bee134daa707d0e7399ce35bb451674f042fbbbcac3f6a3cb77846949c"
            )]
        );
        assert_eq!(bundle.txs.len(), 1);

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
    fn test_correct_bundle_uuid_multiple_reverting_hashes() {
        // reverting tx hashes ordering should not matter
        let inputs = [
            r#"
        {
            "blockNumber": "0x1136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": ["0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        }
        "#,
            r#"
        {
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
                .decode(TxEncoding::WithBlobData)
                .expect("failed to convert bundle request to bundle");

            assert_eq!(
                bundle.hash,
                fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
            );
            assert_eq!(bundle.uuid, uuid!("d9a3ae52-79a2-5ce9-a687-e2aa4183d5c6"));
        }
    }

    #[test]
    fn test_correct_bundle_uuid_no_reverting_hashes() {
        // raw json string
        let bundle_json = r#"
        {
            "blockNumber": "0xA136F1F",
            "txs": ["0x02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1"],
            "revertingTxHashes": []
        }"#;

        let bundle_request: RawBundle =
            serde_json::from_str(bundle_json).expect("failed to decode bundle");

        let bundle = bundle_request
            .decode(TxEncoding::WithBlobData)
            .expect("failed to convert bundle request to bundle");

        assert_eq!(
            bundle.hash,
            fixed_bytes!("cf3c567aede099e5455207ed81c4884f72a4c0c24ddca331163a335525cd22cc")
        );
        assert_eq!(bundle.uuid, uuid!("5d5bf52c-ac3f-57eb-a3e9-fc01b18ca516"));
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
            tx: tx.envelope_encoded(),
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
              "block": "0x1"
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
                      "tx": "0x02f8730180843b9aca00852ecc889a008288b894c10000000000000000000000000000000000000088016345785d8a000080c001a07c8890151fed9a826f241d5a37c84062ebc55ca7f5caef4683dcda6ac99dbffba069108de72e4051a764f69c51a6b718afeff4299107963a5d84d5207b2d6932a4"
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
                "signer": "0x4696595f68034b47BbEc82dB62852B49a8EE7105"
            }
        }
        "#;

        let bundle_request: RawShareBundle =
            serde_json::from_str(bundle_json).expect("failed to decode share bundle");

        let bundle = bundle_request
            .clone()
            .decode_new_bundle(TxEncoding::WithBlobData)
            .expect("failed to convert share bundle request to share bundle");

        let bundle_roundtrip = RawShareBundle::encode_no_blobs(bundle.clone());
        assert_eq!(bundle_request, bundle_roundtrip);

        assert_eq!(bundle.block, 1);
        assert_eq!(bundle.max_block, 1);
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
                    false
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
        assert!(matches!(b.body[1], ShareBundleBody::Tx { .. }));
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
        assert!(matches!(b.body[0], ShareBundleBody::Tx { .. }));
        assert!(matches!(b.body[1], ShareBundleBody::Tx { .. }));
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
}
