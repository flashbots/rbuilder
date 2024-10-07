use tokio::sync::mpsc;
use uuid::Uuid;

use crate::primitives::{Order};

use super::{
    replaceable_order_sink::ReplaceableOrderSink,
    ReplaceableOrderPoolCommand,
};

#[derive(Debug)]
pub struct BobOrderShim {
    sender: mpsc::UnboundedSender<(Order, Uuid)>
}

impl BobOrderShim {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<(Order, Uuid)>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }

}

impl ReplaceableOrderSink for BobOrderShim {
    fn process_command(&mut self, command: ReplaceableOrderPoolCommand) -> bool {
        // TODO: does this need to be a clone?
        match command.clone() {
            ReplaceableOrderPoolCommand::BobOrder((o, uuid)) => {
                self.sender.send((o, uuid)).is_ok()
            }
            _ => true,
        }
    }

    fn is_alive(&self) -> bool {
        return !self.sender.is_closed();
    }
}
