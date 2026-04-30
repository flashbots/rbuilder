fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/quote_api_v1.proto",
        "proto/builder_priority_update_v1.proto",
    ];
    tonic_build::configure().compile_protos(&protos, &["proto"])?;
    Ok(())
}
