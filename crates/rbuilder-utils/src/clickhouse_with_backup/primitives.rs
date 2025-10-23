use alloy_primitives::B256;
use clickhouse::{Row, RowWrite};
use serde::{de::DeserializeOwned, Serialize};

pub trait ClickhouseRowExt:
    Row + RowWrite + Serialize + DeserializeOwned + Sync + Send + 'static
{
    /// The type of such row, e.g. "bundles" or "bundle_receipts". Used as backup db table name and
    /// for informational purposes.
    const ORDER: &'static str;

    /// An identifier of such row.
    fn hash(&self) -> B256;

    /// Internal function that takes the inner row types and extracts the reference needed for
    /// Clickhouse inserter functions like `Inserter::write`. While a default implementation is not
    /// provided, it should suffice to simply return `row`.
    fn to_row_ref(row: &Self) -> &<Self as Row>::Value<'_>;
}

/// An high-level order type that can be indexed in clickhouse.
pub trait ClickhouseIndexableOrder: Sized {
    /// The associated inner row type that can be serialized into Clickhouse data.
    type ClickhouseRowType: ClickhouseRowExt;

    /// The type of such order, e.g. "bundles" or "transactions". For informational purposes.
    const ORDER: &'static str;

    /// An identifier of such order.
    fn hash(&self) -> B256;

    /// Converts such order into the associated Clickhouse row type.
    fn to_row(self, builder_name: String) -> Self::ClickhouseRowType;
}
