//! [Source](https://github.com/op-rs/kona/blob/a1d8ea603960cb4bd3cc19784f7c3365352f1849/crates/protocol/interop/src/safety.rs)
use derive_more::Display;
/// The safety level of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafetyLevel {
    /// The message is finalized.
    Finalized,
    /// The message is safe.
    Safe,
    /// The message is safe locally.
    LocalSafe,
    /// The message is unsafe across chains.
    CrossUnsafe,
    /// The message is unsafe.
    Unsafe,
    /// The message is invalid.
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safety_level_serde() {
        let level = SafetyLevel::Finalized;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, r#""finalized""#);

        let level: SafetyLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(level, SafetyLevel::Finalized);
    }

    #[test]
    fn test_serde_safety_level_fails() {
        let json = r#""failed""#;
        let level: Result<SafetyLevel, _> = serde_json::from_str(json);
        assert!(level.is_err());
    }
}
