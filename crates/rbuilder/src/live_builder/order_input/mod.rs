//! order_input handles receiving new orders from the ipc mempool subscription and json rpc server
//!
pub mod blob_type_order_filter;
pub mod mempool_txs_detector;
pub mod order_replacement_manager;
pub mod order_sink;
pub mod orderpool;
pub mod replaceable_order_sink;
pub mod rpc_server;
pub mod txpool_fetcher;

use self::{
    orderpool::{OrderPool, OrderPoolSubscriptionId},
    replaceable_order_sink::ReplaceableOrderSink,
};
use crate::{
    live_builder::base_config::DEFAULT_TIME_TO_KEEP_MEMPOOL_TXS_SECS,
    provider::StateProviderFactory,
    telemetry::{set_current_block, set_ordepool_stats},
};
use alloy_consensus::Header;
use alloy_primitives::Address;
use jsonrpsee::RpcModule;
use parking_lot::Mutex;
use rbuilder_primitives::{serialize::CancelShareBundle, BundleReplacementData, Order};
use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::base_config::BaseConfig;

/// Thread safe access to OrderPool to get orderflow
#[derive(Debug)]
pub struct OrderPoolSubscriber {
    orderpool: Arc<Mutex<OrderPool>>,
}

impl OrderPoolSubscriber {
    pub fn add_sink(
        &self,
        block_number: u64,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> OrderPoolSubscriptionId {
        self.orderpool.lock().add_sink(block_number, sink)
    }

    pub fn remove_sink(
        &self,
        id: &OrderPoolSubscriptionId,
    ) -> Option<Box<dyn ReplaceableOrderSink>> {
        self.orderpool.lock().remove_sink(id)
    }

    /// Returned AutoRemovingOrderPoolSubscriptionId will call remove when dropped
    pub fn add_sink_auto_remove(
        &self,
        block_number: u64,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> AutoRemovingOrderPoolSubscriptionId {
        AutoRemovingOrderPoolSubscriptionId {
            orderpool: self.orderpool.clone(),
            id: self.add_sink(block_number, sink),
        }
    }
}

/// OrderPoolSubscriptionId that removes on drop.
/// Call add_sink to get flow and remove_sink to stop it
/// For easy auto remove we have add_sink_auto_remove
pub struct AutoRemovingOrderPoolSubscriptionId {
    orderpool: Arc<Mutex<OrderPool>>,
    id: OrderPoolSubscriptionId,
}

impl Drop for AutoRemovingOrderPoolSubscriptionId {
    fn drop(&mut self) {
        self.orderpool.lock().remove_sink(&self.id);
    }
}

#[derive(Debug, Clone)]
pub enum MempoolSource {
    Ipc(PathBuf),
    Ws(String),
}

/// All the info needed to start all the order related jobs (mempool, rcp, clean)
#[derive(Debug, Clone)]
pub struct OrderInputConfig {
    /// if true - cancellations are disabled.
    ignore_cancellable_orders: bool,
    /// if true -- txs with blobs are ignored
    ignore_blobs: bool,
    /// Tx pool source
    mempool_source: Option<MempoolSource>,
    /// Input RPC port
    server_port: u16,
    /// Input RPC ip
    server_ip: Ipv4Addr,
    /// Input RPC max connections
    serve_max_connections: u32,
    /// All order sources send new ReplaceableOrderPoolCommands through an mpsc::Sender bounded channel.
    /// Timeout to wait when sending to that channel (after that the ReplaceableOrderPoolCommand is lost).
    results_channel_timeout: Duration,
    /// Size of the bounded channel.
    pub input_channel_buffer_size: usize,
    /// See [OrderPool::time_to_keep_mempool_txs]
    time_to_keep_mempool_txs: Duration,
    /// The address of coinbase signer for identifying system transactions.
    builder_address: Address,
    /// The allowlisted recipients for system transactions.
    system_recipient_allowlist: Vec<Address>,
}
pub const DEFAULT_SERVE_MAX_CONNECTIONS: u32 = 4096;
pub const DEFAULT_RESULTS_CHANNEL_TIMEOUT: Duration = Duration::from_millis(50);
pub const DEFAULT_INPUT_CHANNEL_BUFFER_SIZE: usize = 10_000;
impl OrderInputConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ignore_cancellable_orders: bool,
        ignore_blobs: bool,
        mempool_source: Option<MempoolSource>,
        server_port: u16,
        server_ip: Ipv4Addr,
        serve_max_connections: u32,
        results_channel_timeout: Duration,
        input_channel_buffer_size: usize,
        time_to_keep_mempool_txs: Duration,
        builder_address: Address,
        system_recipient_allowlist: Vec<Address>,
    ) -> Self {
        Self {
            ignore_cancellable_orders,
            ignore_blobs,
            mempool_source,
            server_port,
            server_ip,
            serve_max_connections,
            results_channel_timeout,
            input_channel_buffer_size,
            time_to_keep_mempool_txs,
            builder_address,
            system_recipient_allowlist,
        }
    }

    pub fn from_config(config: &BaseConfig) -> eyre::Result<Self> {
        let serve_max_connections = config
            .jsonrpc_server_max_connections
            .unwrap_or(DEFAULT_SERVE_MAX_CONNECTIONS);

        let mempool = if let Some(provider) = &config.ipc_provider {
            Some(MempoolSource::Ws(provider.mempool_server_url.clone()))
        } else if let Some(path) = &config.el_node_ipc_path {
            let expanded_path = expand_path(path.as_path())?;
            Some(MempoolSource::Ipc(expanded_path))
        } else {
            None
        };

        Ok(OrderInputConfig {
            ignore_cancellable_orders: config.ignore_cancellable_orders,
            ignore_blobs: config.ignore_blobs,
            mempool_source: mempool,
            server_port: config.jsonrpc_server_port,
            server_ip: config.jsonrpc_server_ip,
            serve_max_connections,
            results_channel_timeout: Duration::from_millis(50),
            input_channel_buffer_size: 10_000,
            time_to_keep_mempool_txs: Duration::from_secs(config.time_to_keep_mempool_txs_secs),
            builder_address: config.coinbase_signer().unwrap().address,
            system_recipient_allowlist: config.system_recipient_allowlist.clone(),
        })
    }

    pub fn default_e2e() -> Self {
        Self {
            mempool_source: Some(MempoolSource::Ipc(PathBuf::from("/tmp/anvil.ipc"))),
            results_channel_timeout: Duration::new(5, 0),
            ignore_cancellable_orders: false,
            ignore_blobs: false,
            input_channel_buffer_size: 10,
            serve_max_connections: DEFAULT_SERVE_MAX_CONNECTIONS,
            server_ip: Ipv4Addr::new(127, 0, 0, 1),
            server_port: 0,
            time_to_keep_mempool_txs: Duration::from_secs(DEFAULT_TIME_TO_KEEP_MEMPOOL_TXS_SECS),
            builder_address: Address::ZERO,
            system_recipient_allowlist: Vec::new(),
        }
    }
}

/// Commands we can get from RPC or mempool fetcher.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[allow(clippy::large_enum_variant)]
pub enum ReplaceableOrderPoolCommand {
    /// New or update order
    Order(Order),
    /// Cancellation for sbundle
    CancelShareBundle(CancelShareBundle),
    CancelBundle(BundleReplacementData),
}

impl ReplaceableOrderPoolCommand {
    pub fn target_block(&self) -> Option<u64> {
        match self {
            ReplaceableOrderPoolCommand::Order(o) => o.target_block(),
            ReplaceableOrderPoolCommand::CancelShareBundle(c) => Some(c.block),
            ReplaceableOrderPoolCommand::CancelBundle(_) => None,
        }
    }
}

/// Starts all the tokio tasks to handle order flow:
/// - Mempool
/// - RPC
/// - Clean up task to remove old stuff.
///
/// @Pending reengineering to modularize rpc, extra_rpc here is a patch to upgrade the created rpc server.
pub async fn start_orderpool_jobs<P>(
    config: OrderInputConfig,
    provider_factory: P,
    extra_rpc: RpcModule<()>,
    global_cancel: CancellationToken,
    order_sender: mpsc::Sender<ReplaceableOrderPoolCommand>,
    order_receiver: mpsc::Receiver<ReplaceableOrderPoolCommand>,
    header_receiver: mpsc::Receiver<Header>,
) -> eyre::Result<(JoinHandle<()>, OrderPoolSubscriber)>
where
    P: StateProviderFactory + 'static,
{
    if config.ignore_cancellable_orders {
        warn!("ignore_cancellable_orders is set to true, some order input is ignored");
    }
    if config.ignore_blobs {
        warn!("ignore_blobs is set to true, some order input is ignored");
    }

    let orderpool = Arc::new(Mutex::new(OrderPool::new(config.time_to_keep_mempool_txs)));
    let subscriber = OrderPoolSubscriber {
        orderpool: orderpool.clone(),
    };

    let clean_job = spawn_clean_orderpool_job(
        header_receiver,
        provider_factory,
        orderpool.clone(),
        global_cancel.clone(),
    )
    .await?;
    let rpc_server = rpc_server::start_server_accepting_bundles(
        config.clone(),
        order_sender.clone(),
        extra_rpc,
        global_cancel.clone(),
    )
    .await?;

    let mut handles = vec![clean_job, rpc_server];

    if config.mempool_source.is_some() {
        info!("Txpool source configured, starting txpool subscription");
        let txpool_fetcher = txpool_fetcher::subscribe_to_txpool_with_blobs(
            config.clone(),
            order_sender.clone(),
            global_cancel.clone(),
        )
        .await?;
        handles.push(txpool_fetcher);
    } else {
        info!("No Txpool source configured, skipping txpool subscription");
    }

    let handle = tokio::spawn(async move {
        info!("OrderPoolJobs: started");

        // @Maybe we should add sleep here because each new order will trigger locking
        let mut new_commands = Vec::new();
        let mut order_receiver: mpsc::Receiver<ReplaceableOrderPoolCommand> = order_receiver;

        loop {
            tokio::select! {
                _ = global_cancel.cancelled() => { break; },
                n = order_receiver.recv_many(&mut new_commands, 100) => {
                    if n == 0 {
                        break;
                    }
                },
            };

            // Ignore orders with cancellations if we can't support them
            if config.ignore_cancellable_orders {
                new_commands.retain(|o| {
                    let cancellable_order = match o {
                        ReplaceableOrderPoolCommand::Order(o) => {
                            if o.replacement_key().is_some() {
                                trace!(order=?o.id(), "Ignoring cancellable order (config: ignore_cancellable_orders)")
                            }
                            o.replacement_key().is_some()
                        },
                        ReplaceableOrderPoolCommand::CancelShareBundle(_)|ReplaceableOrderPoolCommand::CancelBundle(_) => true
                    };
                    !cancellable_order
                })
            }

            if config.ignore_blobs {
                new_commands.retain(|o| {
                    let has_blobs = match o {
                        ReplaceableOrderPoolCommand::Order(o) => {
                            if o.has_blobs() {
                                trace!(order=?o.id(), "Ignoring order with blobs (config: ignore_blobs)");
                            }
                            o.has_blobs()
                        },
                        ReplaceableOrderPoolCommand::CancelShareBundle(_)|ReplaceableOrderPoolCommand::CancelBundle(_) => false
                    };
                    !has_blobs
                })
            }

            {
                let mut orderpool = orderpool.lock();
                orderpool.process_commands(new_commands.clone());
            }
            new_commands.clear();
        }

        for handle in handles {
            handle
                .await
                .map_err(|err| {
                    tracing::error!(?err, "Error while waiting for OrderPoolJobs to finish")
                })
                .unwrap_or_default();
        }
        info!("OrderPoolJobs: finished");
    });

    Ok((handle, subscriber))
}

pub fn expand_path(path: &Path) -> eyre::Result<PathBuf> {
    let path_str = path
        .to_str()
        .ok_or_else(|| eyre::eyre!("Invalid UTF-8 in path"))?;

    Ok(PathBuf::from(shellexpand::full(path_str)?.into_owned()))
}

/// Performs maintenance operations on every new header by calling OrderPool::head_updated.
/// Also calls some functions to generate metrics.
async fn spawn_clean_orderpool_job<P>(
    header_receiver: mpsc::Receiver<Header>,
    provider_factory: P,
    orderpool: Arc<Mutex<OrderPool>>,
    global_cancellation: CancellationToken,
) -> eyre::Result<JoinHandle<()>>
where
    P: StateProviderFactory + 'static,
{
    let mut header_receiver: mpsc::Receiver<Header> = header_receiver;

    let handle = tokio::spawn(async move {
        info!("Clean orderpool job: started");

        loop {
            tokio::select! {
                header = header_receiver.recv() => {
                    if let Some(header) = header {
                        let current_block = header.number;
                        set_current_block(current_block);
                        let state = match provider_factory.latest() {
                            Ok(state) => state,
                            Err(err) => {
                                error!("Failed to get latest state: {}", err);
                                // @Metric error count
                                continue;
                            }
                        };

                        let mut orderpool = orderpool.lock();
                        let start = Instant::now();

                        orderpool.head_updated(current_block, &state);

                        let update_time = start.elapsed();
                        let (tx_count, bundle_count) = orderpool.content_count();
                        set_ordepool_stats(tx_count, bundle_count, orderpool.mempool_txs_size());
                        debug!(
                            current_block,
                            tx_count,
                            bundle_count,
                            update_time_ms = update_time.as_millis(),
                            "Cleaned orderpool",
                        );
                    } else {
                        info!("Clean orderpool job: channel ended");
                        if !global_cancellation.is_cancelled(){
                            error!("Clean orderpool job: channel ended with no cancellation");
                        }
                        break;
                    }
                },
                _ = global_cancellation.cancelled() => {
                    info!("Clean orderpool job: received cancellation signal");
                    break;
                }
            }
        }

        global_cancellation.cancel();
        info!("Clean orderpool job: finished");
    });
    Ok(handle)
}
