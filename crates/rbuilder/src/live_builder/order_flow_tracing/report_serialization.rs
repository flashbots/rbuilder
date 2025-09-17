use crate::live_builder::order_flow_tracing::order_flow_tracer::OrderFlowTracerReport;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::{fs::File, io::Write, path::PathBuf};

/// eyre errors since we usually don't do anything but trace the errors
/// bin serialization + gzip compression
pub fn save_report(report: OrderFlowTracerReport, path: PathBuf) -> eyre::Result<()> {
    let binary_report = bincode::serialize(&report)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&binary_report)?;
    let compressed_binary_report = encoder.finish()?;
    let mut file = File::create(path.clone())?;
    file.write_all(&compressed_binary_report)?;
    Ok(())
}

/// eyre errors since we usually don't do anything but trace the errors
pub fn load_report(path: PathBuf) -> eyre::Result<OrderFlowTracerReport> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let report: OrderFlowTracerReport = bincode::deserialize_from(decoder)?;
    Ok(report)
}
