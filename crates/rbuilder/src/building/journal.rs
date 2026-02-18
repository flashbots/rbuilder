use thiserror::Error;

use crate::live_builder::{payload_events::MevBoostSlotData, simulation::SimulatedOrderCommand};

#[derive(Debug, Error)]
pub enum Error {
    #[error("slot already in progress")]
    SlotAlreadyInProgress,
}

/// Sequence number of the SimulatedOrderCommand in the journal.
/// Starts at 0 and increments by 1 for each SimulatedOrderCommand.
pub type JournalSequenceNumber = usize;
/// SimulatedOrderCommands than enter block building in a sequential way.
#[derive(Clone, Debug)]
pub struct SimulatedOrderJournalCommand {
    command: SimulatedOrderCommand,
    sequence_number: JournalSequenceNumber,
}

impl SimulatedOrderJournalCommand {
    pub fn new(command: SimulatedOrderCommand, sequence_number: JournalSequenceNumber) -> Self {
        Self {
            command,
            sequence_number,
        }
    }

    pub fn sequence_number(&self) -> JournalSequenceNumber {
        self.sequence_number
    }

    pub fn command(&self) -> &SimulatedOrderCommand {
        &self.command
    }
}

pub trait OrderJournalObserverFactory: std::fmt::Debug {
    fn create_observer(
        &self,
        slot_data: &MevBoostSlotData,
    ) -> Result<Box<dyn OrderJournalObserver + Send + Sync>, Error>;
}

pub trait OrderJournalObserver: std::fmt::Debug {
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
