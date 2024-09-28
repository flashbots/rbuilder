//! The main entry point for `op-rbuilder`.
//!
//! `op-rbuilder` is an OP Stack EL client with block building capabilities.
//!
//! It is planned to handle `eth_sendBundle` requests, revert protection, and
//! some other features.

#![cfg_attr(all(not(test), feature = "optimism"), warn(unused_crate_dependencies))]
// The `optimism` feature must be enabled to use this crate.
#![cfg(feature = "optimism")]

mod eth_bundle_api;
mod node;

use tracing as _;

// jemalloc provides better performance
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    node::run()
}
