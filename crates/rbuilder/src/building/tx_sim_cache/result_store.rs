use std::sync::Arc;

use alloy_primitives::{Address, U256};
use revm::state::AccountInfo;

use parking_lot::RwLock;

use super::AccessRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    CheckAccount(Address),
    CheckStorage(Address, U256),
    DoNothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    AccountValue(Option<AccountInfo>),
    StorageValue(U256),
    DoNothing,
}

#[derive(Debug, Clone)]
struct StoredResult<R> {
    trace: Vec<(NextAction, ActionResult)>,
    result: R,
}

#[derive(Debug, Clone)]
pub struct ExecutionResultStore<R> {
    nodes: Arc<RwLock<Vec<Arc<StoredResult<R>>>>>,
}

impl<R> Default for ExecutionResultStore<R> {
    fn default() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl<R: Clone + std::fmt::Debug> ExecutionResultStore<R> {
    pub fn len(&self) -> usize {
        self.nodes.read().len()
    }

    pub fn get_walker(&self) -> ExecutionResultStoreWalker<R> {
        ExecutionResultStoreWalker {
            store: self.clone(),
            trace_idx: 0,
            record_idx: 0,
        }
    }

    pub fn insert_result(&self, access_trace: Vec<AccessRecord>, result: R) {
        let mut trace = Vec::with_capacity(access_trace.len());
        for record in access_trace {
            let trace_entry = match record {
                AccessRecord::Account { address, result } => (
                    NextAction::CheckAccount(address),
                    ActionResult::AccountValue(result),
                ),
                AccessRecord::Storage {
                    address,
                    index,
                    result,
                } => (
                    NextAction::CheckStorage(address, index),
                    ActionResult::StorageValue(result),
                ),
            };
            trace.push(trace_entry);
        }
        if trace.is_empty() {
            trace.push((NextAction::DoNothing, ActionResult::DoNothing));
        }
        {
            // don't store same result twice
            let nodes = self.nodes.read().clone();
            for node in nodes {
                if node.trace == trace {
                    return;
                }
            }
        }
        self.nodes
            .write()
            .push(Arc::new(StoredResult { trace, result }));
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionResultStoreWalker<R> {
    store: ExecutionResultStore<R>,
    trace_idx: usize,
    record_idx: usize,
}

impl<R: Clone + std::fmt::Debug> ExecutionResultStoreWalker<R> {
    fn stored_result(&self, idx: usize) -> Option<Arc<StoredResult<R>>> {
        let nodes = self.store.nodes.read();
        nodes.get(idx).map(Arc::clone)
    }

    pub fn next_action(&self) -> Option<NextAction> {
        let stored_result = self.stored_result(self.trace_idx)?;
        let (action, _result) = stored_result.trace.get(self.record_idx)?;
        Some(action.clone())
    }

    pub fn action_result(&mut self, action_result: &ActionResult) -> Option<R> {
        let stored_result = self.stored_result(self.trace_idx)?;
        let (_action, expected_result) = stored_result
            .trace
            .get(self.record_idx)
            .expect("must exist");
        if action_result == expected_result {
            self.record_idx += 1;
            if self.record_idx == stored_result.trace.len() {
                return Some(stored_result.result.clone());
            }
        } else {
            self.trace_idx += 1;
            self.record_idx = 0;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::test_utils::{addr, u256};

    use super::*;

    type Store = ExecutionResultStore<u64>;

    fn account(address: u64, balance: u64) -> AccessRecord {
        AccessRecord::Account {
            address: addr(address),
            result: Some(AccountInfo {
                balance: u256(balance),
                nonce: 0,
                code_hash: Default::default(),
                code: None,
            }),
        }
    }

    fn storage(address: u64, slot: u64, value: u64) -> AccessRecord {
        AccessRecord::Storage {
            address: addr(address),
            index: u256(slot),
            result: u256(value),
        }
    }

    fn get_storage(trace: &[AccessRecord], address: Address, slot: U256) -> Option<U256> {
        for tr in trace {
            if let AccessRecord::Storage {
                address: rec_address,
                index,
                result,
            } = tr
            {
                if rec_address == &address && index == &slot {
                    return Some(*result);
                }
            }
        }
        None
    }

    fn get_account(trace: &[AccessRecord], address: Address) -> Option<Option<AccountInfo>> {
        for tr in trace {
            if let AccessRecord::Account {
                address: rec_address,
                result,
            } = tr
            {
                if rec_address == &address {
                    return Some(result.clone());
                }
            }
        }
        None
    }

    fn get_result_using_walker(current_state: &[AccessRecord], store: &Store) -> Option<u64> {
        let mut walker = store.get_walker();
        loop {
            let next_action = match walker.next_action() {
                Some(a) => a,
                None => {
                    return None;
                }
            };
            let result = match next_action {
                NextAction::CheckAccount(address) => {
                    let account =
                        get_account(current_state, address).expect("asked about unknown account");
                    ActionResult::AccountValue(account)
                }
                NextAction::CheckStorage(address, slot) => {
                    let storage =
                        get_storage(current_state, address, slot).expect("asked about uknown slot");
                    ActionResult::StorageValue(storage)
                }
                NextAction::DoNothing => ActionResult::DoNothing,
            };
            if let Some(result) = walker.action_result(&result) {
                return Some(result);
            }
        }
    }

    #[test]
    fn test_basic_result_insert() {
        let store = Store::default();

        let mut cached_data: Vec<(_, u64)> = Vec::new();
        let trace = vec![account(0x0, 0), storage(0x0, 0, 0)];
        cached_data.push((trace, 1));
        let trace = vec![
            account(0x0, 1),
            account(0x1, 0),
            storage(0x1, 1, 0),
            storage(0x2, 2, 0),
        ];
        cached_data.push((trace, 2));
        let trace = vec![account(0x0, 1), account(0x1, 0), storage(0x1, 1, 2)];
        cached_data.push((trace.clone(), 3));
        cached_data.push((trace, 3)); // insert the same result again, shoudl be noop

        for (trace, result) in &cached_data {
            store.insert_result(trace.clone(), *result);
        }

        for (state, result) in &cached_data {
            let found = get_result_using_walker(state, &store);
            assert_eq!(*result, found.expect("result not found"));
        }

        let mut non_cached_data = Vec::new();
        let trace = vec![account(0x0, 50)];
        non_cached_data.push(trace);
        let trace = vec![account(0x0, 1), account(0x1, 0), storage(0x1, 1, 120)];
        non_cached_data.push(trace);
        let trace = vec![account(0x0, 1), account(0x1, 0), storage(0x1, 1, 200)];
        non_cached_data.push(trace);

        for state in non_cached_data {
            let found = get_result_using_walker(&state, &store);
            assert_eq!(None, found);
        }
    }

    #[test]
    fn test_empty_result_insert() {
        // this can happen if tx errors before reading any state
        let store = Store::default();

        store.insert_result(Vec::new(), 1);

        let found = get_result_using_walker(&[], &store);
        assert_eq!(1, found.expect("result not found"));
    }

    #[test]
    fn test_len_multiple_same_results() {
        let store = Store::default();
        let trace = vec![account(0x0, 1)];
        store.insert_result(trace.clone(), 1);
        store.insert_result(trace.clone(), 1);

        assert_eq!(1, store.len());

        let found = get_result_using_walker(&trace, &store);
        assert_eq!(1, found.expect("result not found"));
    }
}
