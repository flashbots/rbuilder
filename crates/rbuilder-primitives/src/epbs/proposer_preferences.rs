use alloy_primitives::Address;
use alloy_rpc_types_beacon::BlsSignature;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

/// Proposer preferences broadcast via the `proposer_preferences` gossip topic.
/// from consensus-specs/specs/gloas/p2p-interface.md:
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProposerPreferences {
    ///  slot at which the validator will propose.
    #[serde_as(as = "DisplayFromStr")]
    pub proposal_slot: u64,
    ///validator index of the proposer.
    #[serde_as(as = "DisplayFromStr")]
    pub validator_index: u64,
    /// proposers preferred fee recipient
    pub fee_recipient: Address,
    /// proposer's preferred gas limit
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SignedProposerPreferences {
    pub message: ProposerPreferences,
    pub signature: BlsSignature,
}
