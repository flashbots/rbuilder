//!
//! Backtest app to build a single block in a similar way as we do in live.
//! It gets the orders from a HistoricalDataStorage, simulates the orders and then runs the building algorithms.
//! It outputs the best algorithm (most profit) so we can check for improvements in our [crate::building::builders::BlockBuildingAlgorithm]s
//! BlockBuildingAlgorithm are defined on the config file but selected on the command line via "--builders"
//! Sample call:
//! backtest-build-block --config /home/happy_programmer/config.toml --builders mgp-ordering --builders mp-ordering 19380913 --show-orders --show-missing

use ahash::HashMap;
use alloy_primitives::utils::format_ether;
use rbuilder_config::load_toml_config;
use rbuilder_primitives::OrderId;
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use tokio_util::sync::CancellationToken;

use crate::{
    backtest::{
        restore_landed_orders::{
            restore_landed_orders, sim_historical_block, ExecutedBlockTx, ExecutedTxs,
            SimplifiedOrder,
        },
        BlockData, HistoricalDataStorage, OrdersWithTimestamp,
    },
    building::{builders::mock_block_building_helper::MockRootHasher, BlockBuildingContext},
    live_builder::{block_list_provider::BlockList, cli::LiveBuilderConfig},
    provider::StateProviderFactory,
    utils::{
        mevblocker::get_mevblocker_price, timestamp_as_u64, timestamp_ms_to_offset_datetime,
        ProviderFactoryReopener,
    },
};
use clap::Parser;
use std::{path::PathBuf, str::FromStr, sync::Arc};

use super::backtest_build_block::{run_backtest_build_block, BuildBlockCfg, OrdersSource};

#[derive(Parser, Debug)]
struct ExtraCfg {
    #[clap(long, help = "Show landed block txs values")]
    sim_landed_block: bool,
    #[clap(long, help = "Show missing block txs")]
    show_missing: bool,
    #[clap(long, help = "use only this orders")]
    only_order_ids: Vec<String>,
    #[clap(
        long,
        help = "build block lag (ms)",
        default_value = "0",
        allow_hyphen_values = true
    )]
    block_building_time_ms: i64,
    #[clap(help = "Block Number")]
    block: u64,
}

#[derive(Parser, Debug)]
struct Cli {
    #[command(flatten)]
    pub build_block_cfg: BuildBlockCfg,
    #[command(flatten)]
    pub extra_cfg: ExtraCfg,
}

/// OrdersSource that gets all the bundles from flashbot's infra.
struct LandedBlockFromDBOrdersSource<ConfigType> {
    block_data: BlockData,
    sim_landed_block: bool,
    config: ConfigType,
    blocklist: BlockList,
}

impl<ConfigType: LiveBuilderConfig> LandedBlockFromDBOrdersSource<ConfigType> {
    async fn new(extra_cfg: ExtraCfg, config: ConfigType) -> eyre::Result<Self> {
        let block_data = read_block_data(
            &config.base_config().backtest_fetch_output_file,
            extra_cfg.block,
            extra_cfg
                .only_order_ids
                .iter()
                .map(|id| {
                    OrderId::from_str(id).map_err(|e| eyre::eyre!("invalid order id: {id}: {e}"))
                })
                .collect::<Result<Vec<OrderId>, eyre::Error>>()?,
            extra_cfg.block_building_time_ms,
            extra_cfg.show_missing,
        )
        .await?;
        let blocklist = config
            .base_config()
            .blocklist_provider(CancellationToken::new())
            .await?
            .get_blocklist()?;

        Ok(Self {
            block_data,
            sim_landed_block: extra_cfg.sim_landed_block,
            config,
            blocklist,
        })
    }
}

impl<ConfigType: LiveBuilderConfig>
    OrdersSource<
        ConfigType,
        ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    > for LandedBlockFromDBOrdersSource<ConfigType>
{
    fn available_orders(&self) -> Vec<OrdersWithTimestamp> {
        self.block_data.available_orders.clone()
    }

    fn block_time_as_unix_ms(&self) -> u64 {
        timestamp_as_u64(&self.block_data.onchain_block)
    }

    fn create_provider_factory(
        &self,
    ) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>>
    {
        self.config.base_config().create_reth_provider_factory(true)
    }

    fn create_block_building_context(&self) -> eyre::Result<BlockBuildingContext> {
        let signer = self.config.base_config().coinbase_signer()?;
        let state_provider = self
            .create_provider_factory()?
            .history_by_block_hash(self.block_data.onchain_block.header.parent_hash)?;
        let mev_blocker_price = get_mevblocker_price(state_provider)?;
        Ok(BlockBuildingContext::from_onchain_block(
            self.block_data.onchain_block.clone(),
            self.config.base_config().chain_spec()?,
            None,
            self.blocklist.clone(),
            signer.address,
            self.block_data.winning_bid_trace.proposer_fee_recipient,
            signer,
            Arc::new(MockRootHasher {}),
            self.config.base_config().evm_caching_enable,
            mev_blocker_price,
        ))
    }

    fn print_custom_stats(
        &self,
        provider: ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    ) -> eyre::Result<()> {
        if self.sim_landed_block {
            let tx_sim_results = sim_historical_block(
                provider,
                self.config.base_config().chain_spec()?,
                self.block_data.onchain_block.clone(),
            )?;
            print_onchain_block_data(tx_sim_results, &self.block_data);
        }
        Ok(())
    }

    fn config(&self) -> &ConfigType {
        &self.config
    }
}

/// Reads from HistoricalDataStorage the BlockData for block.
/// only_order_ids: if not empty returns only the given order ids.
/// block_building_time_ms: If not 0, time it took to build the block. It allows us to filter out orders that arrived after we started building the block.
/// show_missing: show on-chain orders that weren't available to us at building time.
async fn read_block_data(
    backtest_fetch_output_file: &PathBuf,
    block: u64,
    only_order_ids: Vec<OrderId>,
    block_building_time_ms: i64,
    show_missing: bool,
) -> eyre::Result<BlockData> {
    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(backtest_fetch_output_file).await?;

    let full_block_data = historical_data_storage.read_block_data(block).await?;
    let orders_cutoff_time = timestamp_ms_to_offset_datetime(
        (full_block_data.winning_bid_trace.timestamp_ms as i64 - block_building_time_ms) as u64,
    );
    let mut block_data = full_block_data.snapshot_including_landed(orders_cutoff_time)?;
    if !only_order_ids.is_empty() {
        block_data.filter_orders_by_ids(&only_order_ids);
    }
    if show_missing {
        show_missing_txs(&block_data);
    }

    println!(
        "Block: {} {:?} landed at {} orders at {}",
        block_data.block_number,
        block_data.onchain_block.header.hash,
        timestamp_ms_to_offset_datetime(full_block_data.winning_bid_trace.timestamp_ms),
        orders_cutoff_time
    );
    println!(
        "bid value: {}",
        format_ether(block_data.winning_bid_trace.value)
    );
    println!(
        "builder pubkey: {:?}",
        block_data.winning_bid_trace.builder_pubkey
    );
    Ok(block_data)
}

fn print_onchain_block_data(tx_sim_results: Vec<ExecutedTxs>, block_data: &BlockData) {
    let mut executed_orders = Vec::new();

    let txs_to_idx: HashMap<_, _> = tx_sim_results
        .iter()
        .enumerate()
        .map(|(idx, tx)| (tx.hash(), idx))
        .collect();

    println!("Onchain block txs:");
    for (idx, tx) in tx_sim_results.into_iter().enumerate() {
        println!(
            "{:>4}, {:>74} revert: {:>5} profit: {}",
            idx,
            tx.hash(),
            !tx.receipt.success,
            format_ether(tx.coinbase_profit)
        );
        if !tx.conflicting_txs.is_empty() {
            println!("   conflicts: ");
        }
        for (tx, slots) in &tx.conflicting_txs {
            for slot in slots {
                println!(
                    "   {:>4} address: {:?>24}, key: {:?}",
                    txs_to_idx.get(tx).unwrap(),
                    slot.address,
                    slot.key
                );
            }
        }
        executed_orders.push(ExecutedBlockTx::new(
            tx.hash(),
            tx.coinbase_profit,
            tx.receipt.success,
        ))
    }

    // restored orders
    let mut simplified_orders = Vec::new();
    for order in block_data.available_orders.iter().map(|os| &os.order) {
        if block_data
            .built_block_data
            .as_ref()
            .map(|bd| bd.included_orders.contains(&order.id()))
            .unwrap_or(true)
        {
            simplified_orders.push(SimplifiedOrder::new_from_order(order));
        }
    }
    let restored_orders = restore_landed_orders(executed_orders, simplified_orders);

    for (id, order) in &restored_orders {
        println!(
            "{:>74} total_profit: {}, unique_profit: {}, error: {:?}",
            id,
            format_ether(order.total_coinbase_profit),
            format_ether(order.unique_coinbase_profit),
            order.error
        );
    }

    if let Some(built_block) = &block_data.built_block_data {
        println!();
        println!("Included orders:");
        for included_order in &built_block.included_orders {
            if let Some(order) = restored_orders.get(included_order) {
                println!(
                    "{:>74} total_profit: {}, unique_profit: {}, error: {:?}",
                    order.order,
                    format_ether(order.total_coinbase_profit),
                    format_ether(order.unique_coinbase_profit),
                    order.error
                );
                for (other, tx) in &order.overlapping_txs {
                    println!("    overlap with: {other:>74} tx {tx:?}");
                }
            } else {
                println!("{included_order:>74} included order not found: ");
            }
        }
    }
}

/// Print information about transactions included on-chain but which are missing in our available orders.
fn show_missing_txs(block_data: &BlockData) {
    let missing_txs = block_data.search_missing_txs_on_available_orders();
    if !missing_txs.is_empty() {
        println!(
            "{} of txs by hashes missing on available orders",
            missing_txs.len()
        );
        for missing_tx in missing_txs.iter() {
            println!("Tx: {missing_tx:?}");
        }
    }
    let missing_nonce_txs = block_data.search_missing_account_nonce_on_available_orders();
    if !missing_nonce_txs.is_empty() {
        println!(
            "\n{} of txs by nonce pairs missing on available orders",
            missing_nonce_txs.len()
        );
        for missing_nonce_tx in missing_nonce_txs.iter() {
            println!(
                "Tx: {:?}, Account: {:?}, Nonce: {}",
                missing_nonce_tx.0, missing_nonce_tx.1.account, missing_nonce_tx.1.nonce,
            );
        }
    }
}

pub async fn run_backtest<ConfigType: LiveBuilderConfig>() -> eyre::Result<()> {
    let cli = Cli::parse();
    let config: ConfigType = load_toml_config(cli.build_block_cfg.config.clone())?;
    let order_source = LandedBlockFromDBOrdersSource::new(cli.extra_cfg, config).await?;
    run_backtest_build_block(cli.build_block_cfg, order_source).await
}
