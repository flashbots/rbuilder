//! Serde helpers for Clickhouse.
pub mod u256 {
    use alloy_primitives::U256;
    use serde::{de::Deserializer, ser::Serializer, Deserialize, Serialize as _};

    /// EVM U256 is represented in big-endian, but ClickHouse expects little-endian.
    pub fn serialize<S: Serializer>(u256: &U256, serializer: S) -> Result<S::Ok, S::Error> {
        let buf: [u8; 32] = u256.to_le_bytes();
        buf.serialize(serializer)
    }

    /// Deserialize U256 following ClickHouse RowBinary format.
    ///
    /// ClickHouse stores U256 in little-endian, we have to convert it back to big-endian.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let buf: [u8; 32] = Deserialize::deserialize(deserializer)?;
        Ok(U256::from_le_bytes(buf))
    }
}

pub mod option_u256 {
    use alloy_primitives::U256;
    use serde::{de::Deserializer, ser::Serializer, Deserialize};

    pub fn serialize<S: Serializer>(
        maybe_u256: &Option<U256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if let Some(u256) = maybe_u256 {
            let buf: [u8; 32] = u256.to_le_bytes();
            serializer.serialize_some(&buf)
        } else {
            serializer.serialize_none()
        }
    }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let option: Option<[u8; 32]> = Deserialize::deserialize(deserializer)?;
        Ok(option.map(U256::from_le_bytes))
    }
}

pub mod vec_u256 {
    use alloy_primitives::U256;
    use serde::{
        de::Deserializer,
        ser::{SerializeSeq, Serializer},
        Deserialize,
    };

    /// Serialize Vec<U256> following ClickHouse RowBinary format.
    ///
    /// EVM U256 is represented in big-endian, but ClickHouse expects little-endian.
    pub fn serialize<S: Serializer>(u256es: &[U256], serializer: S) -> Result<S::Ok, S::Error> {
        // It consists of a LEB128 length prefix followed by the raw bytes of each U256 in
        // little-endian order.

        // <https://github.com/ClickHouse/clickhouse-rs/blob/v0.13.3/src/rowbinary/ser.rs#L159-L164>
        let mut seq = serializer.serialize_seq(Some(u256es.len()))?;
        for u256 in u256es {
            let buf: [u8; 32] = u256.to_le_bytes();
            seq.serialize_element(&buf)?;
        }
        seq.end()
    }

    /// Deserialize Vec<U256> following ClickHouse RowBinary format.
    ///
    /// ClickHouse stores U256 in little-endian, we have to convert it back to big-endian.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<[u8; 32]> = Deserialize::deserialize(deserializer)?;
        Ok(vec.into_iter().map(U256::from_le_bytes).collect())
    }
}
