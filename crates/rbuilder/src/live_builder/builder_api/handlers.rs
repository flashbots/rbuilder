use alloy_primitives::{Address, B256};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use rbuilder_primitives::epbs::{GetBidParams, GetExecutionPayloadBidResponse};
use std::sync::Arc;
use tracing::{error, info, trace};

use super::EpbsBuilderState;

/// GET /eth/v1/builder/execution_payload_bid/{slot}/{parent_hash}/{parent_root}/{proposer_index}
///
/// returns a SignedExecutionPayloadBid for the given slot.
pub async fn get_execution_payload_bid_handler(
    State(state): State<Arc<EpbsBuilderState>>,
    Path((slot, parent_hash, parent_root, proposer_index)): Path<(u64, String, String, u64)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, GetExecutionPayloadBidError> {
    // Parse path parameters
    let parent_hash =
        parse_hash(&parent_hash).map_err(|_| GetExecutionPayloadBidError::InvalidParentHash)?;
    let parent_root =
        parse_hash(&parent_root).map_err(|_| GetExecutionPayloadBidError::InvalidParentRoot)?;

    // Parse headers
    let fee_recipient = parse_fee_recipient(&headers)?;
    let timeout_ms = parse_timeout_ms(&headers);
    let date_milliseconds = parse_date_milliseconds(&headers);

    let params = GetBidParams {
        slot,
        parent_hash: parent_hash.into(),
        parent_root,
        proposer_index,
        fee_recipient,
        timeout_ms,
        date_milliseconds,
    };

    trace!(
        slot = params.slot,
        proposer_index = params.proposer_index,
        ?params.parent_hash,
        ?params.fee_recipient,
        "Received get_execution_payload_bid request"
    );

    // Get the best bid from the builder
    match state.get_execution_payload_bid(&params).await {
        Ok(Some(signed_bid)) => {
            info!(
                slot = params.slot,
                block_hash = ?signed_bid.message.block_hash,
                value = signed_bid.message.value,
                "Returning execution payload bid"
            );

            let response = GetExecutionPayloadBidResponse {
                version: "gloas".to_string(),
                data: signed_bid,
            };

            Ok((
                StatusCode::OK,
                [("Eth-Consensus-Version", "gloas")],
                Json(response),
            ))
        }
        Ok(None) => {
            trace!(slot = params.slot, "No execution payload bid available");
            Err(GetExecutionPayloadBidError::NoBidAvailable)
        }
        Err(e) => {
            error!(slot = params.slot, error = ?e, "Error generating execution payload bid");
            Err(GetExecutionPayloadBidError::InternalError(e.to_string()))
        }
    }
}

/// Error type for get_execution_payload_bid handler.
#[derive(Debug)]
pub enum GetExecutionPayloadBidError {
    InvalidParentHash,
    InvalidParentRoot,
    InvalidFeeRecipient,
    MissingFeeRecipient,
    NoBidAvailable,
    InternalError(String),
}

impl IntoResponse for GetExecutionPayloadBidError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            GetExecutionPayloadBidError::InvalidParentHash => {
                (StatusCode::BAD_REQUEST, "Invalid parent_hash".to_string())
            }
            GetExecutionPayloadBidError::InvalidParentRoot => {
                (StatusCode::BAD_REQUEST, "Invalid parent_root".to_string())
            }
            GetExecutionPayloadBidError::InvalidFeeRecipient => (
                StatusCode::BAD_REQUEST,
                "Invalid X-Fee-Recipient header".to_string(),
            ),
            GetExecutionPayloadBidError::MissingFeeRecipient => (
                StatusCode::BAD_REQUEST,
                "Missing required X-Fee-Recipient header".to_string(),
            ),
            GetExecutionPayloadBidError::NoBidAvailable => {
                // Per spec, return 204 No Content when no bid is available
                return StatusCode::NO_CONTENT.into_response();
            }
            GetExecutionPayloadBidError::InternalError(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        };

        let body = serde_json::json!({
            "code": status.as_u16(),
            "message": message
        });

        (status, Json(body)).into_response()
    }
}

// Helper functions for parsing request parameters

fn parse_hash(s: &str) -> Result<B256, ()> {
    // Strip 0x prefix if present
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|_| ())?;
    if bytes.len() != 32 {
        return Err(());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(B256::from(arr))
}

fn parse_fee_recipient(headers: &HeaderMap) -> Result<Address, GetExecutionPayloadBidError> {
    let header_value = headers
        .get("X-Fee-Recipient")
        .ok_or(GetExecutionPayloadBidError::MissingFeeRecipient)?
        .to_str()
        .map_err(|_| GetExecutionPayloadBidError::InvalidFeeRecipient)?;

    let s = header_value.strip_prefix("0x").unwrap_or(header_value);
    let bytes = hex::decode(s).map_err(|_| GetExecutionPayloadBidError::InvalidFeeRecipient)?;
    if bytes.len() != 20 {
        return Err(GetExecutionPayloadBidError::InvalidFeeRecipient);
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(Address::from(arr))
}

fn parse_timeout_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("X-Timeout-Ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn parse_date_milliseconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("Date-Milliseconds")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// GET /eth/v1/builder/status
///
/// Returns 200 OK if the builder is healthy.
pub async fn status_handler() -> StatusCode {
    StatusCode::OK
}
