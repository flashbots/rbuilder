use std::fmt::Write;

/// Writes indent spaces
pub fn write_indent<Buffer: Write>(indent: usize, buf: &mut Buffer) -> std::fmt::Result {
    buf.write_str(&format!("{: <1$}", "", indent))
}
