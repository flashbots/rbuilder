use alloy_primitives::{Address, B256, U256};
use revm::{
    state::{AccountInfo, Bytecode},
    Database,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccessRecord {
    Account {
        address: Address,
        result: Option<AccountInfo>,
    },
    Storage {
        address: Address,
        index: U256,
        result: U256,
    },
}

#[derive(Debug, Clone)]
pub struct TxStateAccessTrace {
    pub trace: Vec<AccessRecord>,
}

impl TxStateAccessTrace {
    fn new() -> Self {
        Self { trace: Vec::new() }
    }

    fn push(&mut self, record: AccessRecord) {
        self.trace.push(record);
    }
}

/// revm database wrapper that records state access
#[derive(Debug)]
pub struct EVMRecordingDatabase<DB> {
    pub should_record: bool,
    pub inner_db: DB,
    pub recorded_trace: TxStateAccessTrace,
}

impl<DB> EVMRecordingDatabase<DB> {
    pub fn new(inner_db: DB, should_record: bool) -> Self {
        Self {
            inner_db,
            recorded_trace: TxStateAccessTrace::new(),
            should_record,
        }
    }
}

impl<DB: Database> Database for EVMRecordingDatabase<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let result = self.inner_db.basic(address)?;
        if !self.should_record {
            return Ok(result);
        }

        self.recorded_trace.push(AccessRecord::Account {
            address,
            result: result.as_ref().map(|r| r.copy_without_code()),
        });
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner_db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let result = self.inner_db.storage(address, index)?;
        if !self.should_record {
            return Ok(result);
        }
        self.recorded_trace.push(AccessRecord::Storage {
            address,
            index,
            result,
        });
        Ok(result)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner_db.block_hash(number)
    }
}
