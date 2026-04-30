//! Generated protobuf bindings for the user-facing WS quote API and the
//! rbuilder gRPC server. Source `.proto` files
//! live under `crates/rbuilder-primitives/proto/`.

pub mod quote_api_v1 {
    tonic::include_proto!("quote_api.v1");
}

pub mod builder_priority_update_v1 {
    tonic::include_proto!("builder_priority_update.v1");
}
