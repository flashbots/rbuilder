//! This module is responsible for receiving payload data from the CL endpoint
//! and slot data from the relay endpoints and converting that to actionable payload event with
//! all the data filled.

pub mod payload_source;
pub mod relay_epoch_cache;

use crate::{
    beacon_api_client::Client,
    live_builder::{
        payload_events::{
            payload_source::PayloadSourceMuxer,
            relay_epoch_cache::{RelaysForSlotData, SlotData},
        },
        SlotSource,
    },
    primitives::mev_boost::{MevBoostRelayID, MevBoostRelaySlotInfoProvider},
};
use alloy_eips::{merge::SLOT_DURATION, BlockNumHash};
use alloy_primitives::{utils::format_ether, Address, B256, U256};
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::block_list_provider::BlockListProvider;

const RECENTLY_SENT_EVENTS_BUFF: usize = 10;
const NEW_PAYLOAD_RECV_TIMEOUT: Duration = SLOT_DURATION.saturating_mul(2);

/// If connection to the consensus client if broken we wait this time.
/// One slot (12secs) is enough so we don't saturate any resource and we don't miss to many slots.
const CONSENSUS_CLIENT_RECONNECT_WAIT: Duration = SLOT_DURATION;

/// Data about a slot received from relays.
/// Contains the important information needed to build and submit the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MevBoostSlotData {
    /// The .data.payload_attributes.suggested_fee_recipient is replaced
    pub payload_attributes_event: PayloadAttributesEvent,
    pub suggested_gas_limit: u64,
    /// List of relays agreeing to the slot_data. It may not contain all the relays (eg: errors, forks, validators registering only to some relays)
    pub relays: Vec<MevBoostRelayID>,
    pub slot_data: SlotData,
}

impl MevBoostSlotData {
    pub fn parent_block_hash(&self) -> B256 {
        self.payload_attributes_event.data.parent_block_hash
    }

    pub fn parent_block_num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(
            self.payload_attributes_event.data.parent_block_number,
            self.payload_attributes_event.data.parent_block_hash,
        )
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

/// Main high level source of MevBoostSlotData to build blocks.
/// Usage:
/// - Create one via MevBoostSlotDataGenerator::new.
/// - Call MevBoostSlotDataGenerator::spawn.
/// - Poll new slots via the returned UnboundedReceiver on spawn.
/// - If join with spawned task is needed await on the JoinHandle returned by spawn.
pub struct MevBoostSlotDataGenerator {
    cls: Vec<Client>,
    relays: Vec<MevBoostRelaySlotInfoProvider>,
    blocklist_provider: Arc<dyn BlockListProvider>,
    global_cancellation: CancellationToken,
}

impl MevBoostSlotDataGenerator {
    pub fn new(
        cls: Vec<Client>,
        relays: Vec<MevBoostRelaySlotInfoProvider>,
        blocklist_provider: Arc<dyn BlockListProvider>,
        global_cancellation: CancellationToken,
    ) -> Self {
        Self {
            cls,
            relays,
            blocklist_provider,
            global_cancellation,
        }
    }

    /// Spawns the reader task.
    /// It reads from a PayloadSourceMuxer, replaces the fee_recipient/gas_limit with the info from the relays and filters duplicates.
    /// Why the need for replacing fee_recipient?
    ///     MEV-boost was built on top of eth 2.0.
    ///     Usually (without MEV-boost) the CL only notifies the EL for the slots it should build (once every 2 months!).
    ///     When MEV-boost is used, we tell the CL “--always-build-payload” (we are building blocks for ANY validator now!). The CL does
    ///     it, but even with the event being created for every slot, the fee_recipient we get from MEV-Boost might be different so we should always replace it.
    ///     Note that with MEV-boost the validator may change the fee_recipient when registering to the Relays.
    pub fn spawn(self) -> (JoinHandle<()>, mpsc::UnboundedReceiver<MevBoostSlotData>) {
        let relays = RelaysForSlotData::new(&self.relays);

        let (send, receive) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let mut source = PayloadSourceMuxer::new(
                &self.cls,
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

                match check_slot_data_for_blocklist(
                    &mev_boost_slot_data,
                    self.blocklist_provider.as_ref(),
                ) {
                    Ok(can_build) => {
                        if !can_build {
                            continue;
                        }
                    }
                    Err(_) => {
                        // Blocklist errors are FATAL
                        error!("Cancelling building due to blocklist errors on MevBoostSlotDataGenerator");
                        self.global_cancellation.cancel();
                        return;
                    }
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

impl SlotSource for MevBoostSlotDataGenerator {
    fn recv_slot_channel(self) -> mpsc::UnboundedReceiver<MevBoostSlotData> {
        let (_handle, chan) = self.spawn();
        chan
    }
}

/// true->build
/// false->don't build
/// Error crisis, close.
fn check_slot_data_for_blocklist(
    data: &MevBoostSlotData,
    blocklist_provider: &dyn BlockListProvider,
) -> Result<bool, super::block_list_provider::Error> {
    if blocklist_provider.current_list_contains(&data.fee_recipient())? {
        warn!(recipiend=?data.fee_recipient(),"Slot data fee recipient is in the blocklist");
        return Ok(false);
    }
    Ok(true)
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
