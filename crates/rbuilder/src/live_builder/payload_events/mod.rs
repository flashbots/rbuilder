//! This module is responsible for receiving payload data from the CL endpoint
//! and slot data from the relay endpoints and converting that to actionable payload event with
//! all the data filled.

pub mod payload_source;
pub mod relay_epoch_cache;

use crate::{
    live_builder::payload_events::{
        payload_source::PayloadSourceMuxer,
        relay_epoch_cache::{RelaysForSlotData, SlotData},
    },
    primitives::mev_boost::{MevBoostRelay, MevBoostRelayID},
};
use ahash::HashSet;
use alloy_primitives::{utils::format_ether, Address, B256, U256};
use reth::{
    primitives::constants::SLOT_DURATION, rpc::types::beacon::events::PayloadAttributesEvent,
};
use std::{collections::VecDeque, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const RECENTLY_SENT_EVENTS_BUFF: usize = 10;
const NEW_PAYLOAD_RECV_TIMEOUT: Duration = SLOT_DURATION.saturating_mul(2);

/// If connection to the consensus client if broken we wait this time.
/// One slot (12secs) is enough so we don't saturate any resource and we don't miss to many slots.
const CONSENSUS_CLIENT_RECONNECT_WAIT: Duration = SLOT_DURATION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MevBoostSlotData {
    pub payload_attributes_event: PayloadAttributesEvent,
    pub suggested_gas_limit: u64,
    /// List of relays that have this slot registered
    pub relays: Vec<MevBoostRelayID>,
    pub slot_data: SlotData,
}

impl MevBoostSlotData {
    pub fn parent_block_hash(&self) -> B256 {
        self.payload_attributes_event.data.parent_block_hash
    }

    pub fn timestamp(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(
            self.payload_attributes_event.attributes().timestamp as i64,
        )
        .unwrap()
    }

    pub fn block(&self) -> u64 {
        self.payload_attributes_event.data.parent_block_number + 1
    }

    pub fn slot(&self) -> u64 {
        self.payload_attributes_event.data.proposal_slot
    }

    pub fn fee_recipient(&self) -> Address {
        self.payload_attributes_event
            .data
            .payload_attributes
            .suggested_fee_recipient
    }
}

pub struct MevBoostSlotDataGenerator {
    cl_urls: Vec<String>,
    relays: Vec<MevBoostRelay>,
    blocklist: HashSet<Address>,

    global_cancellation: CancellationToken,
}

impl MevBoostSlotDataGenerator {
    pub fn new(
        cl_urls: Vec<String>,
        relays: Vec<MevBoostRelay>,
        blocklist: HashSet<Address>,
        global_cancellation: CancellationToken,
    ) -> Self {
        Self {
            cl_urls,
            relays,
            blocklist,
            global_cancellation,
        }
    }

    pub fn spawn(self) -> (JoinHandle<()>, mpsc::UnboundedReceiver<MevBoostSlotData>) {
        let relays = RelaysForSlotData::new(&self.relays);

        let (send, receive) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let mut source = PayloadSourceMuxer::new(
                &self.cl_urls,
                NEW_PAYLOAD_RECV_TIMEOUT,
                CONSENSUS_CLIENT_RECONNECT_WAIT,
                self.global_cancellation.clone(),
            );

            info!("MevBoostSlotDataGenerator: started");
            let mut relays = relays;
            let mut recently_sent_data = VecDeque::with_capacity(RECENTLY_SENT_EVENTS_BUFF);

            while let Some(event) = source.recv().await {
                if self.global_cancellation.is_cancelled() {
                    return;
                }

                let (slot_data, relays) =
                    if let Some(res) = relays.slot_data(event.data.proposal_slot).await {
                        res
                    } else {
                        continue;
                    };

                let mut correct_event = event;
                correct_event
                    .data
                    .payload_attributes
                    .suggested_fee_recipient = slot_data.fee_recipient;

                let mev_boost_slot_data = MevBoostSlotData {
                    payload_attributes_event: correct_event,
                    suggested_gas_limit: slot_data.gas_limit,
                    relays,
                    slot_data,
                };

                if let Err(err) =
                    check_slot_data_for_blocklist(&mev_boost_slot_data, &self.blocklist)
                {
                    warn!("Slot data failed blocklist check: {:?}", err);
                    continue;
                }

                if recently_sent_data.contains(&mev_boost_slot_data) {
                    continue;
                }
                if recently_sent_data.len() > RECENTLY_SENT_EVENTS_BUFF {
                    recently_sent_data.pop_front();
                }
                recently_sent_data.push_back(mev_boost_slot_data.clone());

                report_slot_withdrawals_to_fee_recipients(&mev_boost_slot_data);

                if send.send(mev_boost_slot_data).is_err() {
                    debug!("MevBoostSlotData events channel closed");
                    break;
                }
            }
            // cancelling here because its a critical job
            self.global_cancellation.cancel();

            source.join().await;
            info!("MevBoostSlotDataGenerator: finished");
        });

        (handle, receive)
    }
}

fn check_slot_data_for_blocklist(
    data: &MevBoostSlotData,
    blocklist: &HashSet<Address>,
) -> eyre::Result<()> {
    if blocklist.contains(&data.fee_recipient()) {
        return Err(eyre::eyre!(
            "Slot data fee recipient is in the blocklist: {:?}",
            data.fee_recipient()
        ));
    }
    Ok(())
}

fn report_slot_withdrawals_to_fee_recipients(data: &MevBoostSlotData) {
    let withdrawals = if let Some(withdrawals) = &data
        .payload_attributes_event
        .data
        .payload_attributes
        .withdrawals
    {
        withdrawals
    } else {
        return;
    };

    let fee_recipient = data.fee_recipient();

    let withdrawals_to_fee_recipient: U256 = withdrawals
        .iter()
        .filter_map(|w| {
            if w.address == fee_recipient {
                Some(w.amount_wei())
            } else {
                None
            }
        })
        .sum();

    if !withdrawals_to_fee_recipient.is_zero() {
        info!(
            slot = data.slot(),
            block = data.block(),
            "Slot has withdrawals to the fee recipient, address: {:?} , amount: {}",
            fee_recipient,
            format_ether(withdrawals_to_fee_recipient)
        );
    }
}
