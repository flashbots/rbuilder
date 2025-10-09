//! Types and utility functions for Optimistic V3 bidding.
use alloy_primitives::B256;
use alloy_rpc_types_beacon::{BlsPublicKey, BlsSignature};
use ethereum_consensus::{
    electra::verify_signed_data, primitives, ssz::prelude::*, Error as ConsensusError,
};

/// Signed get payload v3 request.
#[derive(Debug, serde::Serialize, serde::Deserialize, ssz_derive::Encode, ssz_derive::Decode)]
pub struct SignedGetPayloadV3 {
    /// Get payload request.
    pub message: GetPayloadV3,
    /// Signature from the relay's key that it uses to sign the `get_header`
    /// responses.
    pub signature: BlsSignature,
}

/// Get payload v3 request.
#[derive(Debug, serde::Serialize, serde::Deserialize, ssz_derive::Encode, ssz_derive::Decode)]
pub struct GetPayloadV3 {
    /// Hash of the block header from the `SignedHeaderSubmission`.
    pub block_hash: B256,
    /// Timestamp (in milliseconds) when the relay made this request.
    pub request_ts: u64,
    /// Bls public key of the signing key that was used to create
    /// the `signature` field in `SignedGetPayloadV3`.
    pub relay_public_key: BlsPublicKey,
}

/// Verify relay request signature.
pub fn verify_signed_relay_request(
    request: &SignedGetPayloadV3,
    domain: B256,
) -> Result<(), ConsensusError> {
    let message = ConsensusGetPayloadV3::try_from(&request.message)?;
    let signature = primitives::BlsSignature::try_from(request.signature.as_slice())?;
    verify_signed_data(&message, &signature, &message.relay_public_key, *domain)
}

/// Internal struct for compatibility with `ethereum-consensus` crate.
#[derive(SimpleSerialize)]
struct ConsensusGetPayloadV3 {
    block_hash: primitives::Hash32,
    request_ts: u64,
    relay_public_key: primitives::BlsPublicKey,
}

impl TryFrom<&GetPayloadV3> for ConsensusGetPayloadV3 {
    type Error = ConsensusError;

    fn try_from(value: &GetPayloadV3) -> Result<Self, Self::Error> {
        let relay_public_key =
            primitives::BlsPublicKey::try_from(value.relay_public_key.as_slice())?;
        let block_hash = primitives::Hash32::try_from(value.block_hash.as_slice())
            .map_err(SimpleSerializeError::Deserialize)?;
        Ok(Self {
            request_ts: value.request_ts,
            relay_public_key,
            block_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethereum_consensus::{
        builder::compute_builder_domain, crypto::SecretKey, electra::sign_with_domain,
        state_transition::Context as ConsensusContext,
    };
    use rand::Rng;

    #[test]
    fn signed_get_payload_v3_roundtrip() {
        let mut rng = rand::thread_rng();
        let relay_secret_key = SecretKey::random(&mut rng).unwrap();
        let request = GetPayloadV3 {
            block_hash: B256::random(),
            relay_public_key: BlsPublicKey::from_slice(relay_secret_key.public_key().as_slice()),
            request_ts: rng.gen(),
        };
        let domain = B256::from(compute_builder_domain(&ConsensusContext::for_mainnet()).unwrap());
        let signature = sign_with_domain(
            &ConsensusGetPayloadV3::try_from(&request).unwrap(),
            &relay_secret_key,
            *domain,
        )
        .unwrap();
        let signed_request = SignedGetPayloadV3 {
            signature: BlsSignature::try_from(signature.as_slice()).unwrap(),
            message: request,
        };
        assert!(verify_signed_relay_request(&signed_request, domain).is_ok());
    }
}
