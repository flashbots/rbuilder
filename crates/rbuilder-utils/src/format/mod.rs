/// A trait for types that can be formatted as a human-readable size in bytes.
pub trait FormatBytes {
    fn format_bytes(&self) -> String;
}

impl FormatBytes for u64 {
    fn format_bytes(&self) -> String {
        if *self < 1024 {
            format!("{}B", self)
        } else if *self < 1024 * 1024 {
            format!("{}KiB", self / 1024)
        } else if *self < 1024 * 1024 * 1024 {
            format!("{}MiB", self / 1024 / 1024)
        } else {
            format!("{}GiB", self / 1024 / 1024 / 1024)
        }
    }
}
