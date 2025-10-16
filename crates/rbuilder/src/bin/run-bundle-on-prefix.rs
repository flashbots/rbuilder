//! Experimental tool to test the backruns a given mev share user got.
//! Giver the user tx hash we look for oll the backrun bundles and try to execute them the block prefix just before the user tx.
//! This way we can see how the bundles would be executed on the block prefix and see if the landed backrun really has best refund for the user..
use alloy_primitives::{b256, TxHash, U256};
use alloy_rpc_types::Block;
use clap::Parser;
use itertools::Itertools;
use rbuilder::{
    backtest::{
        execute::{backtest_prepare_orders_from_building_context, BacktestBlockInput},
        BlockData, HistoricalDataStorage, OrdersWithTimestamp,
    },
    building::{
        builders::{
            block_building_helper::{BlockBuildingHelper, BlockBuildingHelperFromProvider},
            BuiltBlockId,
        },
        order_priority::{FullProfitInfoGetter, OrderMaxProfitPriority},
        BlockBuildingContext, ExecutionError, MockRootHasher, NullPartialBlockExecutionTracer,
        OrderPriority, ThreadBlockBuildingContext,
    },
    live_builder::{cli::LiveBuilderConfig, config::Config},
    provider::StateProviderFactory,
    utils::{extract_onchain_block_txs, find_suggested_fee_recipient},
};
use rbuilder_config::load_toml_config;
use rbuilder_primitives::{
    order_statistics::OrderStatistics, MempoolTx, Order, SimValue, SimulatedOrder,
    TransactionSignedEcRecoveredWithBlobs,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
pub struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    pub config: PathBuf,
    #[clap(help = "Block Number")]
    block: u64,
}

struct LandedBlockInfo {
    config: Config,
    block_data: BlockData,
    landed_txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    building_cxt_original_coinbase: BlockBuildingContext,
    building_cxt_cfg_coinbase: BlockBuildingContext,
    pub local_ctx: ThreadBlockBuildingContext,
}

impl LandedBlockInfo {
    pub async fn new(path: impl AsRef<Path>, block: u64) -> eyre::Result<Self> {
        let config: Config = load_toml_config(path)?;
        let mut historical_data_storage =
            HistoricalDataStorage::new_from_path(&config.base_config().backtest_fetch_output_file)
                .await?;
        let block_data = historical_data_storage.read_block_data(block).await?;
        let cut_off_time = block_data
            .built_block_data
            .as_ref()
            .unwrap()
            .orders_closed_at;
        let block_data = block_data.snapshot_including_landed(cut_off_time)?;
        let onchain_block = block_data.onchain_block.clone();
        let landed_txs = extract_onchain_block_txs(&onchain_block)?;
        let building_cxt_original_coinbase =
            Self::create_building_context(&config, &block_data.onchain_block, &landed_txs, true)?;
        let building_cxt_cfg_coinbase =
            Self::create_building_context(&config, &block_data.onchain_block, &landed_txs, false)?;
        Ok(Self {
            config,
            block_data,
            landed_txs,
            local_ctx: ThreadBlockBuildingContext::default(),
            building_cxt_original_coinbase,
            building_cxt_cfg_coinbase,
        })
    }

    /// Filters all orders containing the target tx.
    pub fn filter_backruns_including(&self, target_tx_hash: TxHash) -> Vec<OrdersWithTimestamp> {
        self.block_data
            .available_orders
            .iter()
            .filter(|order| {
                order
                    .order
                    .list_txs()
                    .iter()
                    .map(|(tx, _)| (**tx).hash())
                    .contains(&target_tx_hash)
            })
            .cloned()
            .collect()
    }

    pub fn get_context(&self, use_original_coinbase: bool) -> BlockBuildingContext {
        if use_original_coinbase {
            self.building_cxt_original_coinbase.clone()
        } else {
            self.building_cxt_cfg_coinbase.clone()
        }
    }
    pub fn sim_orders(
        &self,
        orders: Vec<OrdersWithTimestamp>,
        use_original_coinbase: bool,
    ) -> eyre::Result<Vec<Arc<SimulatedOrder>>> {
        let BacktestBlockInput { sim_orders, .. } = backtest_prepare_orders_from_building_context(
            self.get_context(use_original_coinbase),
            orders,
            self.config
                .base_config()
                .create_reth_provider_factory(true)?,
            &self.config.base_config().sbundle_mergeable_signers(),
        )?;
        Ok(sim_orders)
    }

    fn create_building_context(
        config: &Config,
        onchain_block: &Block,
        txs: &[TransactionSignedEcRecoveredWithBlobs],
        use_original_coinbase: bool,
    ) -> eyre::Result<BlockBuildingContext> {
        let suggested_fee_recipient = find_suggested_fee_recipient(onchain_block, txs);
        let signer = config.base_config().coinbase_signer()?;
        // If we put the real coinbase we cant create a signer and we can't pay kickbacks
        let coinbase = if use_original_coinbase {
            onchain_block.header.beneficiary
        } else {
            signer.address
        };
        Ok(BlockBuildingContext::from_onchain_block(
            onchain_block.clone(),
            config.base_config().chain_spec()?,
            None,
            Default::default(),
            coinbase,
            suggested_fee_recipient,
            signer,
            Arc::new(MockRootHasher {}),
            false,
            U256::ZERO,
        ))
    }

    pub fn create_building_helper(
        &mut self,
        use_original_coinbase: bool,
    ) -> eyre::Result<BlockBuildingHelperFromProvider<NullPartialBlockExecutionTracer>> {
        let ctx = self.get_context(use_original_coinbase);
        let provider = self
            .config
            .base_config()
            .create_reth_provider_factory(true)?;
        let block_state = provider
            .history_by_block_hash(ctx.attributes.parent)?
            .into();
        let order_statistics = OrderStatistics::new();
        Ok(BlockBuildingHelperFromProvider::new(
            BuiltBlockId::ZERO,
            block_state,
            ctx,
            &mut self.local_ctx,
            "TEST".to_string(),
            false,
            order_statistics,
            CancellationToken::new(),
            None,
        )?)
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    let mut block_info = LandedBlockInfo::new(cli.config.clone(), cli.block).await?;
    // This is the tx hash of the user tx.
    let target_tx = b256!("0xbe997c5ead2a4f66cbf340d75aaafeb05981da79a24fbd772a732c3a2b11fe85");
    // This are all the backrun bundles that include the user tx.
    let target_orders = block_info.filter_backruns_including(target_tx);

    println!(
        "Original Coinbase: {:?}",
        block_info
            .building_cxt_original_coinbase
            .evm_env
            .block_env
            .beneficiary
    );
    println!("\n=========== LANDED BLOCK");
    for tx in block_info.landed_txs.clone() {
        println!("{:?}", tx.hash());
    }

    println!("\n=========== SIM TARGET ORDERS ToB Exec");
    // Test orders on Tob since in prod we only try to land backruns that worked on ToB.
    execute_orders_on_tob(&target_orders, &mut block_info)?;

    println!("\n=========== RESIM SIMED TARGET ORDERS ToB Exec");
    let sim_orders = block_info.sim_orders(target_orders.clone(), false)?;
    // Re test orders on ToB
    execute_sim_orders_on_tob(&sim_orders, &mut block_info)?;

    println!("=========== BLOCK PREFIX EXEC");
    // Execute block prefix
    let mut builder = block_info.create_building_helper(true)?;

    // Execute prefix (up to the user tx)
    for tx in block_info.landed_txs.clone() {
        if tx.hash() == target_tx {
            break;
        }
        let order = Order::Tx(MempoolTx::new(tx.clone()));
        let sim_order = SimulatedOrder {
            order,
            sim_value: Default::default(),
            used_state_trace: Default::default(),
        };
        let res = builder.commit_order(&mut block_info.local_ctx, &sim_order, &|_| Ok(()))?;
        println!("{:?} {:?}", tx.hash(), res.is_ok());
    }

    // Test backruns after prefix.
    println!("Backruns after prefix");
    for sim_order in sim_orders {
        let mut builder = builder.box_clone();
        let res = builder.commit_order(&mut block_info.local_ctx, &sim_order, &|sim_result| {
            simulation_too_low::<OrderMaxProfitPriority<FullProfitInfoGetter>>(
                &sim_order.sim_value,
                sim_result,
            )
        })?;

        let profit = res
            .as_ref()
            .map(|res| res.coinbase_profit)
            .unwrap_or_default();

        let kickbacks = res
            .as_ref()
            .map(|res| res.paid_kickbacks.clone())
            .unwrap_or_default();

        println!(
            "{:?} {:?} profit {:?} kickbacks{:?}",
            sim_order.order.id(),
            res.is_ok(),
            profit,
            kickbacks
        );
    }

    Ok(())
}

#[allow(clippy::result_large_err)]
fn simulation_too_low<OrderPriorityType: OrderPriority>(
    original_sim_result: &SimValue,
    new_sim_result: &SimValue,
) -> Result<(), ExecutionError> {
    if OrderPriorityType::simulation_too_low(original_sim_result, new_sim_result) {
        Err(ExecutionError::LowerInsertedValue {
            before: original_sim_result.clone(),
            inplace: new_sim_result.clone(),
        })
    } else {
        Ok(())
    }
}

/// Sanity check of the simulation using a BlockBuildingHelper and also allows us to easily check extra stuff not in SimulatedOrder.
fn execute_sim_orders_on_tob(
    sim_orders: &[Arc<SimulatedOrder>],
    block_info: &mut LandedBlockInfo,
) -> eyre::Result<()> {
    // Re test orders on ToB
    for sim_order in sim_orders {
        let mut builder = block_info.create_building_helper(false)?;
        let res = builder.commit_order(&mut block_info.local_ctx, sim_order, &|_| Ok(()))?;
        let profit = res
            .as_ref()
            .map(|res| res.coinbase_profit)
            .unwrap_or_default();
        println!(
            "{:?}  res {:?} profit {:?}",
            sim_order.id(),
            res.is_ok(),
            profit
        );
        println!(
            "    {:?}",
            sim_order
                .order
                .original_orders()
                .iter()
                .map(|o| o.id())
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn execute_orders_on_tob(
    target_orders: &[OrdersWithTimestamp],
    block_info: &mut LandedBlockInfo,
) -> eyre::Result<()> {
    for order_ts in target_orders {
        let mut builder = block_info.create_building_helper(false)?;
        let sim_order = SimulatedOrder {
            order: order_ts.order.clone(),
            sim_value: Default::default(),
            used_state_trace: Default::default(),
        };
        let res = builder.commit_order(&mut block_info.local_ctx, &sim_order, &|_| Ok(()))?;
        let profit = res
            .as_ref()
            .map(|res| res.coinbase_profit)
            .unwrap_or_default();
        println!(
            "{:?} signer {:?} res {:?} profit {:?} Txs: {:?}",
            order_ts.order.id(),
            order_ts.order.signer(),
            res.is_ok(),
            profit,
            order_ts.order.list_txs(),
        );
    }
    Ok(())
}
