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
pub async fn get_bid_handler(
    State(state): State<Arc<EpbsBuilderState>>,
    Path((slot, parent_hash, parent_root, proposer_index)): Path<(u64, String, String, u64)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, GetBidError> {
    // Parse path parameters
    let parent_hash = parse_hash(&parent_hash).map_err(|_| GetBidError::InvalidParentHash)?;
    let parent_root = parse_hash(&parent_root).map_err(|_| GetBidError::InvalidParentRoot)?;

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
        "Received get_bid request"
    );

    // Get the best bid from the builder
    match state.get_bid(&params).await {
        Ok(Some(signed_bid)) => {
            info!(
                slot = params.slot,
                block_hash = ?signed_bid.message.block_hash,
                value = signed_bid.message.value,
                "Returning bid"
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
            trace!(slot = params.slot, "No bid available");
            Err(GetBidError::NoBidAvailable)
        }
        Err(e) => {
            error!(slot = params.slot, error = ?e, "Error generating bid");
            Err(GetBidError::InternalError(e.to_string()))
        }
    }
}

/// Error type for get_bid handler.
#[derive(Debug)]
pub enum GetBidError {
    InvalidParentHash,
    InvalidParentRoot,
    InvalidFeeRecipient,
    MissingFeeRecipient,
    NoBidAvailable,
    InternalError(String),
}

impl IntoResponse for GetBidError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            GetBidError::InvalidParentHash => {
                (StatusCode::BAD_REQUEST, "Invalid parent_hash".to_string())
            }
            GetBidError::InvalidParentRoot => {
                (StatusCode::BAD_REQUEST, "Invalid parent_root".to_string())
            }
            GetBidError::InvalidFeeRecipient => (
                StatusCode::BAD_REQUEST,
                "Invalid X-Fee-Recipient header".to_string(),
            ),
            GetBidError::MissingFeeRecipient => (
                StatusCode::BAD_REQUEST,
                "Missing required X-Fee-Recipient header".to_string(),
            ),
            GetBidError::NoBidAvailable => {
                // acc spec return 204 No Content when no bid is available
                return StatusCode::NO_CONTENT.into_response();
            }
            GetBidError::InternalError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
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

fn parse_fee_recipient(headers: &HeaderMap) -> Result<Address, GetBidError> {
    let header_value = headers
        .get("X-Fee-Recipient")
        .ok_or(GetBidError::MissingFeeRecipient)?
        .to_str()
        .map_err(|_| GetBidError::InvalidFeeRecipient)?;

    let s = header_value.strip_prefix("0x").unwrap_or(header_value);
    let bytes = hex::decode(s).map_err(|_| GetBidError::InvalidFeeRecipient)?;
    if bytes.len() != 20 {
        return Err(GetBidError::InvalidFeeRecipient);
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



