use clap::Parser;
use reth::cli::Cli;
use reth_node_optimism::{args::RollupArgs, OptimismNode};
use reth_optimism_rpc::eth::rpc::SequencerClient;
use reth_rpc_api::EthBundleApiServer;

use crate::eth_bundle_api;

pub fn run() {
    if let Err(err) = Cli::<RollupArgs>::parse().run(|builder, rollup_args| async move {
        let sequencer_http_arg = rollup_args.sequencer_http.clone();
        let handle = builder
            .node(OptimismNode::new(rollup_args.clone()))
            .extend_rpc_modules(move |ctx| {
                // register sequencer tx forwarder
                if let Some(sequencer_http) = sequencer_http_arg {
                    ctx.registry
                        .eth_api()
                        .set_sequencer_client(SequencerClient::new(sequencer_http));
                }

                // register eth bundle api
                let ext = eth_bundle_api::EthBundleApi {};
                ctx.modules.merge_configured(ext.into_rpc())?;

                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
