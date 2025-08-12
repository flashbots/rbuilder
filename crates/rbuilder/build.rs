fn main() -> Result<(), Box<dyn std::error::Error>> {
    built::write_built_file().expect("Failed to acquire build-time information");

    // Generate bloxroute gRPC bindings.
    let proto_path: &std::path::Path = "proto/bloxroute.proto".as_ref();
    let proto_dir = proto_path
        .parent()
        .expect("proto file should reside in a directory");
    tonic_build::configure()
        .disable_package_emission()
        .compile_protos(&[proto_path], &[proto_dir])?;

    Ok(())
}
