//! The main entry point for `op-rbuilder`.
//!
//! `op-rbuilder` is an OP Stack EL client with block building capabilities.
//!
//! It is planned to handle `eth_sendBundle` requests, revert protection, and
//! some other features.

mod eth_bundle_api;
mod node;

// jemalloc provides better performance
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(not(feature = "optimism"))]
compile_error!("Cannot build the `op-rbuilder` binary without the `optimism` feature flag enabled. Did you mean to build `rbuilder`?");

#[cfg(feature = "optimism")]
fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    node::run()
}
