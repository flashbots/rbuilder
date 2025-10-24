pub mod backup;
pub mod indexer;
use serde::{Deserialize, Serialize};

/// Equilalent of `clickhouse::inserter::Quantities` with more traits derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Quantities {
    pub bytes: u64,
    pub rows: u64,
    pub transactions: u64,
}

impl Quantities {
    /// Just zero quantities, nothing special.
    pub const ZERO: Quantities = Quantities {
        bytes: 0,
        rows: 0,
        transactions: 0,
    };
}

impl From<clickhouse::inserter::Quantities> for Quantities {
    fn from(value: clickhouse::inserter::Quantities) -> Self {
        Self {
            bytes: value.bytes,
            rows: value.rows,
            transactions: value.transactions,
        }
    }
}

impl From<Quantities> for clickhouse::inserter::Quantities {
    fn from(value: Quantities) -> Self {
        Self {
            bytes: value.bytes,
            rows: value.rows,
            transactions: value.transactions,
        }
    }
}
