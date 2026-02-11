use crate::live_builder::{payload_events::MevBoostSlotData, simulation::SimulatedOrderCommand};

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

pub trait OrderJournalObserver: std::fmt::Debug {
    fn order_delivered(&self, slot_data: &MevBoostSlotData, command: &SimulatedOrderJournalCommand);
}

#[derive(Debug)]
pub struct NullOrderJournalObserver {}

impl OrderJournalObserver for NullOrderJournalObserver {
    fn order_delivered(
        &self,
        _slot_data: &MevBoostSlotData,
        _command: &SimulatedOrderJournalCommand,
    ) {
    }
}
