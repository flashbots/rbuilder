use crate::{
    metrics::{
        add_payload_processing_time, add_payload_validation_time, add_winning_bid,
        inc_payload_validation_errors, inc_payloads_received, inc_relay_errors,
    },
    validation_api_client::{ValidationAPIClient, ValidationError},
};
use ahash::HashMap;
use alloy_consensus::proofs::calculate_withdrawals_root;
use alloy_primitives::{bytes::Bytes, utils::format_ether, B256, U256};
use flate2::bufread::GzDecoder;
use parking_lot::Mutex;
use rbuilder::{
    beacon_api_client::Client,
    live_builder::{
        block_list_provider::NullBlockListProvider,
        payload_events::{MevBoostSlotData, MevBoostSlotDataGenerator},
    },
    mev_boost::MevBoostRelaySlotInfoProvider,
};
use rbuilder_primitives::mev_boost::SubmitBlockRequest;
use serde::{Deserialize, Serialize};
use ssz::Decode as _;
use std::{
    collections::hash_map::Entry,
    io::Read,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use warp::{
    body,
    http::status::StatusCode,
    query,
    reply::{self, Reply},
    Filter,
};

#[derive(Debug)]
enum RelayError {
    ValidatorsFetch,
    InvalidParams,
    BlockProcessing,
    FailedToAcceptSubmission,
    PastSlot,
    PayloadAttributesNotKnown,
    SimulationFailed(String),
}

impl RelayError {
    fn reply(self) -> Box<dyn Reply> {
        let (code, internal, message) = match self {
            RelayError::ValidatorsFetch => (1, false, "Failed to fetch validators".to_string()),
            RelayError::InvalidParams => (2, false, "Invalid request params".to_string()),
            RelayError::BlockProcessing => (3, true, "Error processing block".to_string()),
            RelayError::FailedToAcceptSubmission => {
                (4, false, "Failed to accept submission".to_string())
            }
            RelayError::PastSlot => (5, false, "submission for past slot".to_string()),
            RelayError::PayloadAttributesNotKnown => {
                (6, false, "payload attributes not (yet) known".to_string())
            }
            RelayError::SimulationFailed(msg) => (7, false, format!("simulation failed: {msg}")),
        };
        let json = RelayErrorResponse { code, message };
        let status_code = if internal {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::BAD_REQUEST
        };
        Box::new(reply::with_status(reply::json(&json), status_code))
    }
}

#[derive(Debug, Serialize)]
struct RelayErrorResponse {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct BlockQueryParams {
    cancellations: Option<i32>,
}

/// It will run the following processes:
/// * API server
/// * mev boost slot info generator that will fetch data from CL nodes + relay
/// * winner sampler that will sample top bid to estimate auction winner for the given slot
pub fn spawn_relay_server(
    address: SocketAddr,
    validation_client: Option<ValidationAPIClient>,
    cl_clients: Vec<Client>,
    relay: MevBoostRelaySlotInfoProvider,
    builder_names: HashMap<String, String>,
    cancellation_token: CancellationToken,
) -> eyre::Result<()> {
    let relay_state = RelayState {
        validation_client,
        relay_for_slot_data: relay.clone(),
        pending_slot_data: Arc::new(Mutex::new(None)),
        builder_names,
    };
    spawn_mev_boost_slot_data_generator(
        relay_state.clone(),
        cl_clients,
        relay,
        cancellation_token.clone(),
    )?;

    tokio::spawn(run_winner_sampler(relay_state.clone(), cancellation_token));

    let relay_handler = warp::any().map(move || relay_state.clone());

    let validators_route = warp::path!("relay" / "v1" / "builder" / "validators")
        .and(warp::get())
        .and(relay_handler.clone())
        .then(|state: RelayState| async move { state.handle_validators().await });

    let submit_block_route = warp::path!("relay" / "v1" / "builder" / "blocks")
        .and(warp::post())
        .and(relay_handler.clone())
        .and(query::query::<BlockQueryParams>())
        .and(body::content_length_limit(20 * 1024 * 1024))
        .and(body::bytes())
        .and(warp::header::<String>("content-type"))
        .and(warp::header::optional::<String>("content-encoding"))
        .then(
            |state: RelayState, query, body, content_type, content_encoding| async move {
                state
                    .handle_block(query, body, content_type, content_encoding)
                    .await
            },
        );

    let routes = submit_block_route.or(validators_route);
    tokio::spawn(warp::serve(routes).run(address));

    Ok(())
}

#[derive(Debug, Clone)]
struct RelayState {
    // Validation client that is used to validate blocks.
    validation_client: Option<ValidationAPIClient>,
    // Relay used to fetch data for /relay/v1/builder/validators
    relay_for_slot_data: MevBoostRelaySlotInfoProvider,
    // Slot data for the last payload arguments received from CL nodes and relay
    pending_slot_data: Arc<Mutex<Option<PendingSlotData>>>,
    builder_names: HashMap<String, String>,
}

impl RelayState {
    async fn handle_validators(&self) -> Box<dyn Reply> {
        match self
            .relay_for_slot_data
            .get_current_epoch_validators()
            .await
        {
            Ok(slot_data) => Box::new(reply::json(&slot_data)),
            Err(err) => {
                warn!(?err, "Failed to fetch epoch data from relay");
                inc_relay_errors();
                RelayError::ValidatorsFetch.reply()
            }
        }
    }

    async fn handle_block(
        &self,
        query: BlockQueryParams,
        body: Bytes,
        content_type: String,
        content_encoding: Option<String>,
    ) -> Box<dyn Reply> {
        let processing_start = Instant::now();
        let cancel = match query.cancellations {
            Some(1) => true,
            Some(0) | None => false,
            _ => return RelayError::InvalidParams.reply(),
        };

        let ssz = match content_type.as_str() {
            "application/octet-stream" => true,
            "application/json" => false,
            _ => return RelayError::InvalidParams.reply(),
        };

        let gzip = match content_encoding.as_deref() {
            Some("gzip") => true,
            None => false,
            _ => return RelayError::InvalidParams.reply(),
        };

        let body_size = body.len();

        let body = if gzip {
            let mut result = Vec::new();
            let mut decoder = GzDecoder::new(body.as_ref());
            match decoder.read_to_end(&mut result) {
                Ok(_) => {}
                Err(err) => {
                    warn!(?err, "Failed to ungzip body");
                    return RelayError::BlockProcessing.reply();
                }
            }
            result.into()
        } else {
            body
        };

        let payload_size = body.len();

        let submission: SubmitBlockRequest = if ssz {
            match SubmitBlockRequest::from_ssz_bytes(body.as_ref()) {
                Ok(res) => res,
                Err(err) => {
                    warn!(?err, "Failed to parse ssz block submission");
                    return RelayError::BlockProcessing.reply();
                }
            }
        } else {
            match serde_json::from_slice(body.as_ref()) {
                Ok(res) => res,
                Err(err) => {
                    warn!(?err, "Failed to parse json block submission");
                    return RelayError::BlockProcessing.reply();
                }
            }
        };

        let builder_id = builder_id(
            submission.bid_trace().builder_pubkey.as_ref(),
            &self.builder_names,
        );

        inc_payloads_received(&builder_id);

        let (withdrawals_root, registered_gas_limit, parent_beacon_block_root) = {
            let pending_slot = self.pending_slot_data.lock();
            let pending_slot = if pending_slot.is_some() {
                pending_slot.as_ref().unwrap()
            } else {
                return RelayError::PayloadAttributesNotKnown.reply();
            };
            if let Err(err) = pending_slot.is_submission_for_this_slot(&submission) {
                return err.reply();
            }

            let withdrawals_root = pending_slot.withdrawals_root;
            let registered_gas_limit = pending_slot.slot_data.suggested_gas_limit;
            let parent_beacon_block_root = pending_slot
                .slot_data
                .payload_attributes_event
                .attributes()
                .parent_beacon_block_root;
            (
                withdrawals_root,
                registered_gas_limit,
                parent_beacon_block_root,
            )
        };

        if let Some(validation) = &self.validation_client {
            let validation_start = Instant::now();
            match validation
                .validate_block(
                    &submission,
                    registered_gas_limit,
                    withdrawals_root,
                    parent_beacon_block_root,
                    CancellationToken::default(),
                )
                .await
            {
                Ok(_) => {}
                Err(ValidationError::ValidationFailed(payload)) => {
                    error!(err = ?payload, "Block validation failed");
                    inc_payload_validation_errors(&builder_id);
                    let msg = serde_json::to_string(&payload).unwrap_or_default();
                    return RelayError::SimulationFailed(msg).reply();
                }
                Err(err) => {
                    warn!(?err, "Unable to validate block");
                    return RelayError::BlockProcessing.reply();
                }
            }

            add_payload_validation_time(validation_start.elapsed());
        }

        let bid_trace = submission.bid_trace();

        debug!(
            body_size,
            payload_size,
            slot = bid_trace.slot,
            value = format_ether(bid_trace.value),
            gas_used = bid_trace.gas_used,
            builder = builder_id,
            ssz,
            gzip,
            "Received a block"
        );

        {
            let mut pending_slot = self.pending_slot_data.lock();
            let pending_slot = if pending_slot.is_some() {
                pending_slot.as_mut().unwrap()
            } else {
                return RelayError::FailedToAcceptSubmission.reply();
            };
            pending_slot.add_new_submission(submission, cancel);
        }

        add_payload_processing_time(processing_start.elapsed());

        Box::new(reply::reply())
    }
}

fn spawn_mev_boost_slot_data_generator(
    relay_state: RelayState,
    cl_clients: Vec<Client>,
    relay: MevBoostRelaySlotInfoProvider,
    cancellation_token: CancellationToken,
) -> eyre::Result<()> {
    let slot_data_generator = MevBoostSlotDataGenerator::new(
        cl_clients,
        vec![relay],
        Duration::from_millis(1_000),
        Default::default(),
        Arc::new(NullBlockListProvider::default()),
        cancellation_token.clone(),
    );
    let (_, slot_data_generator) = slot_data_generator.spawn();

    tokio::spawn(run_slot_data_fetcher(
        relay_state,
        slot_data_generator,
        cancellation_token,
    ));
    Ok(())
}

async fn run_slot_data_fetcher(
    relay_state: RelayState,
    mut slot_data_generator: mpsc::UnboundedReceiver<MevBoostSlotData>,
    cancellation_token: CancellationToken,
) {
    'slot_data: loop {
        let new_slot_data = tokio::select! {
            data = slot_data_generator.recv() => if let Some(data) = data {
                data
            } else {
                break 'slot_data;
            },
            _ = cancellation_token.cancelled() => break 'slot_data,
        };
        {
            info!(slot = new_slot_data.slot(), "New slot data");
            let mut current_slot_data = relay_state.pending_slot_data.lock();
            *current_slot_data = Some(PendingSlotData::new(new_slot_data));
        }
    }
}

async fn run_winner_sampler(relay_state: RelayState, cancellation_token: CancellationToken) {
    const WINNER_SAMPLING_INTERVAL: Duration = Duration::from_millis(50);
    const SAMPLING_RANGE_DELTA: time::Duration = time::Duration::seconds(2);

    'sampling: loop {
        tokio::select! {
                _ = tokio::time::sleep(WINNER_SAMPLING_INTERVAL) => {},
                _ = cancellation_token.cancelled() => break 'sampling,
        };
        {
            let current_slot_data = relay_state.pending_slot_data.lock();

            let (slot_end, best_bid) = if let Some(value) = current_slot_data
                .as_ref()
                .map(|s| (s.slot_data.timestamp(), &s.best_bid))
            {
                value
            } else {
                continue 'sampling;
            };

            let best_bid = if let Some(best_bid) = best_bid {
                best_bid
            } else {
                continue 'sampling;
            };

            let now = OffsetDateTime::now_utc();

            let too_early = now < slot_end - SAMPLING_RANGE_DELTA;
            let too_late = slot_end + SAMPLING_RANGE_DELTA < now;

            if too_early || too_late {
                continue 'sampling;
            }

            let builder = builder_id(&best_bid.builder, &relay_state.builder_names);
            add_winning_bid(&builder, best_bid.advantage);
        }
    }
}

#[derive(Debug)]
struct PendingSlotData {
    // Payload attributes for the slot
    slot_data: MevBoostSlotData,
    // Withdrawals root is calculated from slot_data and used for validation call.
    withdrawals_root: B256,
    // Current best bid on the relay
    // its calculated from best_bid_by_replacement_key by taking the highest value bid
    best_bid: Option<BestBidData>,
    // Best bid by builders.
    // There are two types of bids: cancellable and not and
    // we must store last cancellable bid by builder pubkey and replace when new bid arrives.
    // For empty key we store uncancallable bids.
    // For non-empty keys we use builder pubkeys.
    best_bid_by_replacement_key: HashMap<Bytes, BestBidData>,
}

impl PendingSlotData {
    fn new(slot_data: MevBoostSlotData) -> Self {
        let withdrawals_root = calculate_withdrawals_root(
            &slot_data
                .payload_attributes_event
                .attributes()
                .withdrawals
                .clone()
                .unwrap_or_default(),
        );
        Self {
            slot_data,
            withdrawals_root,
            best_bid: None,
            best_bid_by_replacement_key: HashMap::default(),
        }
    }

    fn is_submission_for_this_slot(
        &self,
        submission: &SubmitBlockRequest,
    ) -> Result<(), RelayError> {
        let bid_trace = submission.bid_trace();
        let relay_slot = self.slot_data.slot();
        let bid_slot = bid_trace.slot;

        #[allow(clippy::comparison_chain)]
        if relay_slot > bid_slot {
            return Err(RelayError::PastSlot);
        } else if relay_slot < bid_slot {
            return Err(RelayError::PayloadAttributesNotKnown);
        }
        if self.slot_data.parent_block_hash() != bid_trace.parent_hash {
            return Err(RelayError::PayloadAttributesNotKnown);
        }
        Ok(())
    }

    fn add_new_submission(&mut self, submission: SubmitBlockRequest, cancellable: bool) {
        if self.is_submission_for_this_slot(&submission).is_err() {
            return;
        }

        let trace = submission.bid_trace();
        let new_bid = BestBidData {
            hash: trace.block_hash,
            value: trace.value,
            builder: trace.builder_pubkey.as_slice().to_vec().into(),
            advantage: 0.0,
        };

        let update_best_bid = if cancellable {
            match self
                .best_bid_by_replacement_key
                .entry(new_bid.builder.clone())
            {
                Entry::Occupied(mut prev_entry) => {
                    let replacing_best_bid = self
                        .best_bid
                        .as_ref()
                        .map(|best_bid| {
                            let replaced_bid = prev_entry.get();
                            best_bid.builder == replaced_bid.builder
                                && best_bid.hash == replaced_bid.hash
                        })
                        .unwrap_or(false);
                    prev_entry.insert(new_bid);
                    replacing_best_bid
                }
                Entry::Vacant(vacant) => {
                    let value_is_higher = self
                        .best_bid
                        .as_ref()
                        .map(|best_bid| new_bid.value > best_bid.value)
                        .unwrap_or(true);
                    vacant.insert(new_bid);
                    value_is_higher
                }
            }
        } else {
            let value_is_higher = self
                .best_bid
                .as_ref()
                .map(|best_bid| new_bid.value > best_bid.value)
                .unwrap_or(true);
            match self.best_bid_by_replacement_key.entry(Bytes::default()) {
                Entry::Occupied(mut prev_entry) => {
                    if new_bid.value > prev_entry.get().value {
                        prev_entry.insert(new_bid);
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(new_bid);
                }
            }
            value_is_higher
        };

        if update_best_bid {
            self.best_bid = self
                .best_bid_by_replacement_key
                .values()
                .max_by_key(|b| b.value)
                .cloned();

            // calculate new best bid advantage
            let best_bid = if let Some(best_bid) = self.best_bid.as_mut() {
                best_bid
            } else {
                return;
            };
            let best_bid_value = best_bid.value;
            let next_best_bid_value = self
                .best_bid_by_replacement_key
                .values()
                .filter(|bid| bid.builder != best_bid.builder)
                .map(|bid| bid.value)
                .max()
                .unwrap_or_default();
            let advantage = if best_bid_value.is_zero()
                || next_best_bid_value.is_zero()
                || best_bid_value <= next_best_bid_value
            {
                0.0
            } else {
                let advantage = (best_bid_value * U256::from(100u64)) / next_best_bid_value
                    - U256::from(100u64);
                advantage.into()
            };
            best_bid.advantage = advantage;
        }
    }
}

#[derive(Debug, Clone)]
struct BestBidData {
    hash: B256,
    value: U256,
    builder: Bytes,
    // advantage is percentage of value that this builder bid has over the next best bid by other builders
    advantage: f64,
}

/// short readable builder id for metrics
fn builder_id(pubkey: &[u8], builder_names: &HashMap<String, String>) -> String {
    let pubkey_hex = alloy_primitives::hex::encode(pubkey);
    if pubkey_hex.len() < 8 {
        return "incorrect_pubkey".to_string();
    }

    let pubkey_name = format!(
        "{}..{}",
        &pubkey_hex[0..4],
        &pubkey_hex[pubkey_hex.len() - 4..]
    );

    if let Some(name) = builder_names.get(&pubkey_name) {
        name.clone()
    } else {
        pubkey_name
    }
}
