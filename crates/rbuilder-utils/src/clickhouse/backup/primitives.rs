use clickhouse::{Row, RowWrite};
use serde::{de::DeserializeOwned, Serialize};

pub trait ClickhouseRowExt:
    Row + RowWrite + Serialize + DeserializeOwned + Sync + Send + 'static
{
    /// Type of
    type TraceId: std::fmt::Display + Send + Sync;

    /// The type of such row, e.g. "bundles" or "bundle_receipts". Used as backup db table name and
    /// for informational purposes.
    const TABLE_NAME: &'static str;

    /// An identifier of such row.
    fn trace_id(&self) -> Self::TraceId;

    /// Internal function that takes the inner row types and extracts the reference needed for
    /// Clickhouse inserter functions like `Inserter::write`. While a default implementation is not
    /// provided, it should suffice to simply return `row`.
    fn to_row_ref(row: &Self) -> &<Self as Row>::Value<'_>;
}

/// An high-level order type that can be indexed in clickhouse.
pub trait ClickhouseIndexableData: Sized {
    /// The associated inner row type that can be serialized into Clickhouse data.
    type ClickhouseRowType: ClickhouseRowExt;

    /// The type of such order, e.g. "bundles" or "transactions". For informational purposes.
    const DATA_NAME: &'static str;

    /// An identifier of such element for when we need to trace it.
    fn trace_id(&self) -> <Self::ClickhouseRowType as ClickhouseRowExt>::TraceId;

    /// Converts such order into the associated Clickhouse row type.
    fn to_row(self, builder_name: String) -> Self::ClickhouseRowType;
}
