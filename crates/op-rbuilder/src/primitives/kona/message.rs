//! [Source](https://github.com/op-rs/kona/blob/a1d8ea603960cb4bd3cc19784f7c3365352f1849/crates/protocol/interop/src/message.rs)

use alloy_primitives::{keccak256, Bytes, Log};
use alloy_sol_types::{sol};
use derive_more::{AsRef, From};

sol! {
    /// @notice The struct for a pointer to a message payload in a remote (or local) chain.
    #[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct MessageIdentifier {
        address origin;
        uint256 blockNumber;
        uint256 logIndex;
        uint256 timestamp;
        #[serde(rename = "chainID")]
        uint256 chainId;
    }

    /// @notice Emitted when a cross chain message is being executed.
    /// @param payloadHash Hash of message payload being executed.
    /// @param identifier Encoded Identifier of the message.
    ///
    /// Parameter names are derived from the `op-supervisor` JSON field names.
    /// See the relevant definition in the Optimism repository:
    /// [Ethereum-Optimism/op-supervisor](https://github.com/ethereum-optimism/optimism/blob/4ba2eb00eafc3d7de2c8ceb6fd83913a8c0a2c0d/op-supervisor/supervisor/types/types.go#L61-L64).
    #[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    event ExecutingMessage(bytes32 indexed payloadHash, MessageIdentifier identifier);

    /// @notice Executes a cross chain message on the destination chain.
    /// @param _id      Identifier of the message.
    /// @param _target  Target address to call.
    /// @param _message Message payload to call target with.
    function executeMessage(
        MessageIdentifier calldata _id,
        address _target,
        bytes calldata _message
    ) external;
}

/// A [RawMessagePayload] is the raw payload of an initiating message.
#[derive(Debug, Clone, From, AsRef, PartialEq, Eq)]
pub struct RawMessagePayload(Bytes);

impl From<&Log> for RawMessagePayload {
    fn from(log: &Log) -> Self {
        let mut data = vec![0u8; log.topics().len() * 32 + log.data.data.len()];
        for (i, topic) in log.topics().iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(topic.as_ref());
        }
        data[(log.topics().len() * 32)..].copy_from_slice(log.data.data.as_ref());
        data.into()
    }
}

impl From<Vec<u8>> for RawMessagePayload {
    fn from(data: Vec<u8>) -> Self {
        Self(Bytes::from(data))
    }
}

impl From<executeMessageCall> for ExecutingMessage {
    fn from(call: executeMessageCall) -> Self {
        Self {
            identifier: call._id,
            payloadHash: keccak256(call._message.as_ref()),
        }
    }
}
