use ahash::HashMap;
use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::AccessList;
use reth_primitives::{Recovered, TransactionSigned};
use revm::{
    bytecode::opcode,
    context::ContextTr,
    inspector::JournalExt,
    interpreter::{interpreter_types::Jumps, CallInputs, CallOutcome, Interpreter},
    Inspector,
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

impl UsedStateTrace {
    /// Order of appending traces matters. We assume that "other" trace comes after previously appended traces.
    /// We keep track of first read and last write operations.
    pub fn append_trace(&mut self, other: &UsedStateTrace) {
        for (read_slot, read_value) in &other.read_slot_values {
            if self.read_slot_values.contains_key(read_slot) {
                continue;
            }
            self.read_slot_values.insert(read_slot.clone(), *read_value);
        }

        self.written_slot_values
            .extend(other.written_slot_values.clone());

        for (address, balance) in &other.read_balances {
            if self.read_balances.contains_key(address) {
                continue;
            }
            self.read_balances.insert(*address, *balance);
        }

        for (address, received_amount) in &other.received_amount {
            *self.received_amount.entry(*address).or_default() += received_amount;
        }

        for (address, sent_amount) in &other.sent_amount {
            *self.sent_amount.entry(*address).or_default() += sent_amount;
        }

        self.created_contracts
            .extend(other.created_contracts.clone());

        for address in &other.destructed_contracts {
            if self.destructed_contracts.contains(address) {
                continue;
            }
            self.destructed_contracts.push(*address);
        }
    }

    pub fn clear(&mut self) {
        self.read_slot_values.clear();
        self.written_slot_values.clear();
        self.read_balances.clear();
        self.received_amount.clear();
        self.sent_amount.clear();
        self.created_contracts.clear();
        self.destructed_contracts.clear();
    }
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
    fn use_tx_nonce(&mut self, tx: &Recovered<TransactionSigned>) {
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

impl<CTX> Inspector<CTX> for UsedStateEVMInspector<'_>
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn step(&mut self, interpreter: &mut Interpreter, _context: &mut CTX) {
        match std::mem::take(&mut self.next_step_action) {
            NextStepAction::ReadSloadKeyResult(slot) => {
                if let Ok(value) = interpreter.stack.peek(0) {
                    let value = B256::from(value.to_be_bytes());
                    let key = SlotKey {
                        address: interpreter.input.target_address,
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
        match interpreter.bytecode.opcode() {
            opcode::SLOAD => {
                if let Ok(slot) = interpreter.stack.peek(0) {
                    let slot = B256::from(slot.to_be_bytes());
                    self.next_step_action = NextStepAction::ReadSloadKeyResult(slot);
                }
            }
            opcode::SSTORE => {
                if let (Ok(slot), Ok(value)) =
                    (interpreter.stack.peek(0), interpreter.stack.peek(1))
                {
                    let written_value = B256::from(value.to_be_bytes());
                    let key = SlotKey {
                        address: interpreter.input.target_address,
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
                if let Ok(addr) = interpreter.stack.peek(0) {
                    let addr = Address::from_word(B256::from(addr.to_be_bytes()));
                    self.next_step_action = NextStepAction::ReadBalanceResult(addr);
                }
            }
            opcode::SELFBALANCE => {
                let addr = interpreter.input.target_address;
                self.next_step_action = NextStepAction::ReadBalanceResult(addr);
            }
            _ => (),
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
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
        _context: &mut CTX,
        _: &revm::interpreter::CreateInputs,
        outcome: &mut revm::interpreter::CreateOutcome,
    ) {
        if let Some(addr) = outcome.address {
            self.used_state_trace.created_contracts.push(addr);
        }
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
        tx: &Recovered<TransactionSigned>,
        used_state_trace: Option<&'a mut UsedStateTrace>,
    ) -> Self {
        let access_list_inspector =
            AccessListInspector::new(tx.access_list().cloned().unwrap_or_default());

        let mut used_state_inspector = used_state_trace.map(UsedStateEVMInspector::new);
        if let Some(i) = &mut used_state_inspector {
            i.use_tx_nonce(tx);
        }

        Self {
            access_list_inspector,
            used_state_inspector,
        }
    }

    pub fn into_access_list(self) -> AccessList {
        self.access_list_inspector.into_access_list()
    }
}

impl<'a, CTX> Inspector<CTX> for RBuilderEVMInspector<'a>
where
    CTX: ContextTr<Journal: JournalExt>,
    UsedStateEVMInspector<'a>: Inspector<CTX>,
{
    #[inline]
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.access_list_inspector.step(interp, context);
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.step(interp, context);
        }
    }

    #[inline]
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.call(context, inputs)
        } else {
            None
        }
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &revm::interpreter::CreateInputs,
        outcome: &mut revm::interpreter::CreateOutcome,
    ) {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.create_end(context, inputs, outcome);
        }
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if let Some(used_state_inspector) = &mut self.used_state_inspector {
            used_state_inspector.selfdestruct(contract, target, value);
        }
    }
}
