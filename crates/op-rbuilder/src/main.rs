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

use std::path::Path;

use crate::eth_bundle_api::EthCallBundleMinimalApiServer;
use clap_builder::Parser;
use eth_bundle_api::EthBundleMinimalApi;
use op_rbuilder_node_optimism::{args::OpRbuilderArgs, OpRbuilderNode};
use reth::cli::Cli;
use reth_node_optimism::node::OptimismAddOns;
use reth_optimism_rpc::eth::rpc::SequencerClient;
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

    hello_op_rbuilder();

    let path = Path::new("/Users/liamaharon/Library/Application Support/reth/901/db/");

    // Succeeds
    reth_db::open_db_read_only(path, Default::default()).unwrap();

    if let Err(err) = Cli::<OpRbuilderArgs>::parse().run(|builder, op_rbuilder_args| async move {
        let sequencer_http_arg = op_rbuilder_args.sequencer_http.clone();

        // Fails
        // reth_db::open_db_read_only(path, Default::default()).unwrap();

        let handle = builder
            .with_types::<OpRbuilderNode>()
            .with_components(OpRbuilderNode::components(op_rbuilder_args))
            .with_add_ons::<OptimismAddOns>()
            .extend_rpc_modules(move |ctx| {
                // register sequencer tx forwarder
                if let Some(sequencer_http) = sequencer_http_arg {
                    ctx.registry
                        .eth_api()
                        .set_sequencer_client(SequencerClient::new(sequencer_http));
                }

                // register eth bundle api
                let ext = EthBundleMinimalApi::new(ctx.registry.pool().clone());
                ctx.modules.merge_configured(ext.into_rpc())?;

                Ok(())
            })
            .launch()
            .await?;

        // Fails
        // reth_db::open_db_read_only(path, Default::default()).unwrap();

        handle.node_exit_future.await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }

    // Succeeds
    reth_db::open_db_read_only(path, Default::default()).unwrap();
}

fn hello_op_rbuilder() {
    println!(
        r#"

 ░▒▓██████▓▒░░▒▓███████▓▒░       ░▒▓███████▓▒░░▒▓███████▓▒░░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓███████▓▒░░▒▓████████▓▒░▒▓███████▓▒░
░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░
░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░
░▒▓█▓▒░░▒▓█▓▒░▒▓███████▓▒░       ░▒▓███████▓▒░░▒▓███████▓▒░░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓██████▓▒░ ░▒▓███████▓▒░
░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░             ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░
░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░             ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░
 ░▒▓██████▓▒░░▒▓█▓▒░             ░▒▓█▓▒░░▒▓█▓▒░▒▓███████▓▒░ ░▒▓██████▓▒░░▒▓█▓▒░▒▓████████▓▒░▒▓███████▓▒░░▒▓████████▓▒░▒▓█▓▒░░▒▓█▓▒░

   ______           ________)
  (, /    )        (, /     /)        /)   /)
    /---(            /___, // _   _  (/   (/_ ____/_ _
 ) / ____) (_/_   ) /     (/_(_(_/_)_/ )_/_) (_) (__/_)_
(_/ (     .-/    (_/
         (_/
    "#
    );
}
