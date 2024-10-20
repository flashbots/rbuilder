use ahash::HashMap;
use alloy_primitives::{Address, B256, U256};
use reth_primitives::TransactionSignedEcRecovered;
use revm::{
    interpreter::{opcode, CallInputs, CallOutcome, Interpreter},
    Database, EvmContext, Inspector,
};
use revm_inspectors::access_list::AccessListInspector;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotKey {
    pub address: Address,
    pub key: B256,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// UsedStateTrace is an execution trace of the given order
/// Limitations:
/// * `written_slot_values`, `received_amount` and `sent_amount` are not correct if transaction reverts
pub struct UsedStateTrace {
    /// read slot values contains first read
    pub read_slot_values: HashMap<SlotKey, B256>,
    /// write slot values contains last write
    pub written_slot_values: HashMap<SlotKey, B256>,
    /// balance of first read
    pub read_balances: HashMap<Address, U256>,
    /// number of `wei` sent or received during execution
    pub received_amount: HashMap<Address, U256>,
    pub sent_amount: HashMap<Address, U256>,
    pub created_contracts: Vec<Address>,
    pub destructed_contracts: Vec<Address>,
}

#[derive(Debug, Clone, Default)]
enum NextStepAction {
    #[default]
    None,
    ReadSloadKeyResult(B256),
    ReadBalanceResult(Address),
}

#[derive(Debug)]
struct UsedStateEVMInspector<'a> {
    next_step_action: NextStepAction,
    used_state_trace: &'a mut UsedStateTrace,
}

impl<'a> UsedStateEVMInspector<'a> {
    fn new(used_state_trace: &'a mut UsedStateTrace) -> Self {
        Self {
            next_step_action: NextStepAction::None,
            used_state_trace,
        }
    }

    /// This method is used to mark nonce change as a slot read / write.
    /// Txs with the same nonce are in conflict and origin address is EOA that does not have storage.
    /// We convert nonce change to the slot 0 read and write of the signer
    fn use_tx_nonce(&mut self, tx: &TransactionSignedEcRecovered) {
        self.used_state_trace.read_slot_values.insert(
            SlotKey {
                address: tx.signer(),
                key: Default::default(),
            },
            U256::from(tx.nonce()).into(),
        );
        self.used_state_trace.written_slot_values.insert(
            SlotKey {
                address: tx.signer(),
                key: Default::default(),
            },
            U256::from(tx.nonce() + 1).into(),
        );
    }
}

impl<'a, DB> Inspector<DB> for UsedStateEVMInspector<'a>
where
    DB: Database,
{
    fn step(&mut self, interpreter: &mut Interpreter, _: &mut EvmContext<DB>) {
        match std::mem::take(&mut self.next_step_action) {
            NextStepAction::ReadSloadKeyResult(slot) => {
                if let Ok(value) = interpreter.stack.peek(0) {
                    let value = B256::from(value.to_be_bytes());
                    let key = SlotKey {
                        address: interpreter.contract.target_address,
                        key: slot,
                    };
                    self.used_state_trace
                        .read_slot_values
                        .entry(key)
                        .or_insert(value);
                }
            }
            NextStepAction::ReadBalanceResult(addr) => {
                if let Ok(value) = interpreter.stack.peek(0) {
                    self.used_state_trace
                        .read_balances
                        .entry(addr)
                        .or_insert(value);
                }
            }
            NextStepAction::None => {}
        }
        match interpreter.current_opcode() {
            opcode::SLOAD => {
                if let Ok(slot) = interpreter.stack().peek(0) {
                    let slot = B256::from(slot.to_be_bytes());
                    self.next_step_action = NextStepAction::ReadSloadKeyResult(slot);
                }
            }
            opcode::SSTORE => {
                if let (Ok(slot), Ok(value)) =
                    (interpreter.stack().peek(0), interpreter.stack().peek(1))
                {
                    let written_value = B256::from(value.to_be_bytes());
                    let key = SlotKey {
                        address: interpreter.contract.target_address,
                        key: B256::from(slot.to_be_bytes()),
                    };
                    // if we write the same value that we read as the first read we don't have a write
                    if let Some(read_value) = self.used_state_trace.read_slot_values.get(&key) {
                        if read_value == &written_value {
                            self.used_state_trace.written_slot_values.remove(&key);
                            return;
                        }
                    }
                    self.used_state_trace
                        .written_slot_values
                        .insert(key, written_value);
                }
            }
            opcode::BALANCE => {
                if let Ok(addr) = interpreter.stack().peek(0) {
                    let addr = Address::from_word(B256::from(addr.to_be_bytes()));
                    self.next_step_action = NextStepAction::ReadBalanceResult(addr);
                }
            }
            opcode::SELFBALANCE => {
                let addr = interpreter.contract().target_address;
                self.next_step_action = NextStepAction::ReadBalanceResult(addr);
            }
            _ => (),
        }
    }

    fn call(&mut self, _: &mut EvmContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if let Some(transfer_value) = inputs.transfer_value() {
            if !transfer_value.is_zero() {
                *self
                    .used_state_trace
                    .sent_amount
                    .entry(inputs.transfer_from())
                    .or_default() += transfer_value;
                *self
                    .used_state_trace
                    .received_amount
                    .entry(inputs.transfer_to())
                    .or_default() += transfer_value;
            }
        }
        None
    }

    fn create_end(
        &mut self,
        _: &mut EvmContext<DB>,
        _: &revm::interpreter::CreateInputs,
        outcome: revm::interpreter::CreateOutcome,
    ) -> revm::interpreter::CreateOutcome {
        if let Some(addr) = outcome.address {
            self.used_state_trace.created_contracts.push(addr);
        }
        outcome
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        // selfdestruct can be called multiple times during transaction execution
        if self
            .used_state_trace
            .destructed_contracts
            .contains(&contract)
        {
            return;
        }
        self.used_state_trace.destructed_contracts.push(contract);
        if !value.is_zero() {
            *self
                .used_state_trace
                .sent_amount
                .entry(contract)
                .or_default() += value;
            *self
                .used_state_trace
                .received_amount
                .entry(target)
                .or_default() += value;
        }
    }
}

#[derive(Debug)]
pub struct RBuilderEVMInspector<'a> {
    access_list_inspector: AccessListInspector,
    used_state_inspector: Option<UsedStateEVMInspector<'a>>,
}

impl<'a> RBuilderEVMInspector<'a> {
    pub fn new(
        tx: &TransactionSignedEcRecovered,
        used_state_trace: Option<&'a mut UsedStateTrace>,
    ) -> Self {
        let access_list_inspector = AccessListInspector::new(
            tx.as_eip2930()
                .map(|tx| tx.access_list.clone())
                .unwrap_or_default(),
            tx.signer(),
            tx.to().unwrap_or_default(),
            None,
        );

        let mut used_state_inspector = used_state_trace.map(UsedStateEVMInspector::new);
        if let Some(i) = &mut used_state_inspector {
            i.use_tx_nonce(tx);
        }

        Self {
            access_list_inspector,
            used_state_inspector,
        }
    }

    pub fn into_access_list(self) -> reth::rpc::types::AccessList {
        self.access_list_inspector.into_access_list()
    }
}

impl<'a, DB> Inspector<DB> for RBuilderEVMInspector<'a>
where
    DB: Database,
    UsedStateEVMInspector<'a>: Inspector<DB>,
{
    #[inline]
    fn step(&mut self, interp: &mut Interpreter, data: &mut EvmContext<DB>) {
        self.access_list_inspector.step(interp, data);
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.step(interp, data);
        }
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut EvmContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.call(context, inputs)
        } else {
            None
        }
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut EvmContext<DB>,
        inputs: &revm::interpreter::CreateInputs,
        outcome: revm::interpreter::CreateOutcome,
    ) -> revm::interpreter::CreateOutcome {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.create_end(context, inputs, outcome)
        } else {
            outcome
        }
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.selfdestruct(contract, target, value)
        }
    }
}
