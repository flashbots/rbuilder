use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    built::write_built_file().expect("Failed to acquire build-time information");
    let proto_file = "./src/bidding_service_wrapper/proto/bidding_service.proto";
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional") // for older systems
        .build_client(true)
        .build_server(true)
        .file_descriptor_set_path(out_dir.join("bidding_service_descriptor.bin"))
        .out_dir("./src/bidding_service_wrapper")
        .compile_protos(&[proto_file], &["proto"])?;
    Ok(())
}
