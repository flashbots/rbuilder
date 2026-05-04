use thiserror::Error;

use crate::live_builder::{payload_events::MevBoostSlotData, simulation::SimulatedOrderCommand};

#[derive(Debug, Error)]
pub enum Error {
    #[error("slot already in progress")]
    SlotAlreadyInProgress,
}

/// Sequence number of the SimulatedOrderCommand in the journal.
/// Independent per [`JournalLane`] — `Main` and `Pu` each maintain their own
/// monotonic counter.
pub type JournalSequenceNumber = usize;

/// Journal lane identifier. Lanes have independent sequence numbers and may
/// carry different command sources (regular sim pipeline vs. priority-update
/// scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalLane {
    /// Standard simulation pipeline (mempool / RPC bundles).
    Main,
    /// Priority-update simulation results.
    Pu,
}

/// SimulatedOrderCommand together with the lane it was delivered on and the
/// per-lane sequence number assigned at delivery time.
#[derive(Clone, Debug)]
pub struct SimulatedOrderJournalCommand {
    command: SimulatedOrderCommand,
    sequence_number: JournalSequenceNumber,
    lane: JournalLane,
}

impl SimulatedOrderJournalCommand {
    pub fn new(
        command: SimulatedOrderCommand,
        sequence_number: JournalSequenceNumber,
        lane: JournalLane,
    ) -> Self {
        Self {
            command,
            sequence_number,
            lane,
        }
    }

    pub fn sequence_number(&self) -> JournalSequenceNumber {
        self.sequence_number
    }

    pub fn command(&self) -> &SimulatedOrderCommand {
        &self.command
    }

    pub fn lane(&self) -> JournalLane {
        self.lane
    }
}

pub trait OrderJournalObserverFactory: std::fmt::Debug {
    fn create_observer(
        &self,
        slot_data: &MevBoostSlotData,
    ) -> Result<Box<dyn OrderJournalObserver + Send + Sync>, Error>;
}

pub trait OrderJournalObserver: std::fmt::Debug {
    /// Called for every delivered command across all lanes — implementations
    /// dispatch on `command.lane()` if they need lane-specific handling.
    fn order_delivered(&self, command: &SimulatedOrderJournalCommand);
}

#[derive(Debug)]
pub struct NullOrderJournalObserverFactory {}

impl OrderJournalObserverFactory for NullOrderJournalObserverFactory {
    fn create_observer(
        &self,
        _slot_data: &MevBoostSlotData,
    ) -> Result<Box<dyn OrderJournalObserver + Send + Sync>, Error> {
        Ok(Box::new(NullOrderJournalObserver {}))
    }
}

#[derive(Debug)]
pub struct NullOrderJournalObserver {}

impl OrderJournalObserver for NullOrderJournalObserver {
    fn order_delivered(&self, _command: &SimulatedOrderJournalCommand) {}
}
