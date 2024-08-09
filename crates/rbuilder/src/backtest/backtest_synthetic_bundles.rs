use crate::{
    backtest::{
        fetch::{
            csv::CSVDatasource,
            datasource::{BlockRef, DataSource},
        },
        OrdersWithTimestamp,
    },
    building::{
        builders::BacktestSimulateBlockInput,
        sim::simulate_all_orders_with_sim_tree,
        testing::test_chain_state::{BlockArgs, ContractData, TestChainState},
    },
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig},
    primitives::{Order, OrderId, SimulatedOrder},
};
use alloy_primitives::Bytes;
use alloy_primitives::{address, utils::format_ether, Address};
use clap::Parser;
use hex::FromHex;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(
        long,
        help = "build block lag (ms)",
        default_value = "0",
        allow_hyphen_values = true
    )]
    block_building_time_ms: i64,
    #[clap(long, help = "Show all available orders")]
    show_orders: bool,
    #[clap(long, help = "Show order data and top of block simulation results")]
    show_sim: bool,
    #[clap(long, help = "Show missing block txs")]
    show_missing: bool,
    #[clap(long, help = "don't build block")]
    no_block_building: bool,
    #[clap(
        long,
        help = "builders to build block with (see config builders)",
        default_value = "mp-ordering"
    )]
    builders: Vec<String>,
    #[clap(long, help = "use only this orders")]
    only_order_ids: Vec<String>,
    #[clap(help = "Block Number")]
    block: u64,
    #[clap(long, help = "csv file path", env = "rbuilder_csv")]
    csv: PathBuf,
}

const MAX_PAYMENT: u128 = 100_000_000_000_000_000; // 0.1 ETH
/// Secret precompiled code.
const STORAGE_CONTENTION_SIMULATOR_CODE_HEX: &str = "0x608060405234801561001057600080fd5b50600436106100885760003560e01c80633b45d7031161005b5780633b45d703146100fa578063b60ab2a81461010d578063b82a721b14610120578063d252124f1461013357600080fd5b806311d5d1f41461008d5780631b5ac4b5146100a25780632123c06a146100c757806326ccecfd146100e7575b600080fd5b6100a061009b366004610679565b610146565b005b6100b56100b03660046106c5565b610200565b60405190815260200160405180910390f35b6100b56100d53660046106c5565b60009081526020819052604090205490565b6100a06100f536600461072a565b61021e565b6100a061010836600461072a565b610350565b6100a061011b366004610835565b610475565b6100a061012e366004610835565b61052d565b6100a0610141366004610870565b61060d565b41318581036101e957604051600090419089908381818185875af1925050503d8060008114610191576040519150601f19603f3d011682016040523d82523d6000602084013e610196565b606091505b50509050806101e35760405162461bcd60e51b81526020600482015260146024820152732330b4b632b2103a379039b2b7321022ba3432b960611b60448201526064015b60405180910390fd5b506101f6565b6101f6888686868661052d565b5050505050505050565b600080821261020f5781610218565b610218826108a8565b92915050565b88831461026d5760405162461bcd60e51b815260206004820181905260248201527f536c6f747320616e642076616c756573206c656e677468206d69736d6174636860448201526064016101da565b88871461028c5760405162461bcd60e51b81526004016101da906108c4565b8885146102ab5760405162461bcd60e51b81526004016101da9061090a565b60005b898110156103435761033b8b8b838181106102cb576102cb610950565b905060200201358a8a848181106102e4576102e4610950565b905060200201358989858181106102fd576102fd610950565b9050602002013588888681811061031657610316610950565b9050602002013587878781811061032f5761032f610950565b9050602002013561052d565b6001016102ae565b5050505050505050505050565b88831461039f5760405162461bcd60e51b815260206004820181905260248201527f536c6f747320616e642076616c756573206c656e677468206d69736d6174636860448201526064016101da565b8887146103be5760405162461bcd60e51b81526004016101da906108c4565b8885146103dd5760405162461bcd60e51b81526004016101da9061090a565b60005b898110156103435761046d8b8b838181106103fd576103fd610950565b905060200201358a8a8481811061041657610416610950565b9050602002013589898581811061042f5761042f610950565b9050602002013588888681811061044857610448610950565b9050602002013587878781811061046157610461610950565b90506020020135610475565b6001016103e0565b6000858152602081905260409020548481108015906104945750838111155b6104b05760405162461bcd60e51b81526004016101da90610966565b60008681526020819052604080822085905551419184156108fc02918591818181858888f193505050501580156104eb573d6000803e3d6000fd5b5060408051878152602081018590527f1224c0b1b1fc9b317194a91a14e45a82b4a46b12e44848ddf8f20f5f69567fcd910160405180910390a1505050505050565b6000858152602081905260408120549061054a6100b085846109bb565b905085821015801561055c5750848211155b6105785760405162461bcd60e51b81526004016101da90610966565b6000878152602081905260409020849055416108fc6105988360016109e2565b6105a29086610a04565b6040518115909202916000818181858888f193505050501580156105ca573d6000803e3d6000fd5b5060408051888152602081018690527f1224c0b1b1fc9b317194a91a14e45a82b4a46b12e44848ddf8f20f5f69567fcd910160405180910390a150505050505050565b600082815260208190526040812054906106278383610a1b565b6000858152602081815260409182902083905581518781529081018390529192507f1224c0b1b1fc9b317194a91a14e45a82b4a46b12e44848ddf8f20f5f69567fcd910160405180910390a150505050565b600080600080600080600060e0888a03121561069457600080fd5b505085359760208701359750604087013596606081013596506080810135955060a0810135945060c0013592509050565b6000602082840312156106d757600080fd5b5035919050565b60008083601f8401126106f057600080fd5b50813567ffffffffffffffff81111561070857600080fd5b6020830191508360208260051b850101111561072357600080fd5b9250929050565b60008060008060008060008060008060a08b8d03121561074957600080fd5b8a3567ffffffffffffffff81111561076057600080fd5b61076c8d828e016106de565b909b5099505060208b013567ffffffffffffffff81111561078c57600080fd5b6107988d828e016106de565b90995097505060408b013567ffffffffffffffff8111156107b857600080fd5b6107c48d828e016106de565b90975095505060608b013567ffffffffffffffff8111156107e457600080fd5b6107f08d828e016106de565b90955093505060808b013567ffffffffffffffff81111561081057600080fd5b61081c8d828e016106de565b915080935050809150509295989b9194979a5092959850565b600080600080600060a0868803121561084d57600080fd5b505083359560208501359550604085013594606081013594506080013592509050565b6000806040838503121561088357600080fd5b50508035926020909101359150565b634e487b7160e01b600052601160045260246000fd5b6000600160ff1b82016108bd576108bd610892565b5060000390565b60208082526026908201527f536c6f747320616e64206c6f77657220626f756e6473206c656e677468206d696040820152650e6dac2e8c6d60d31b606082015260800190565b60208082526026908201527f536c6f747320616e6420757070657220626f756e6473206c656e677468206d696040820152650e6dac2e8c6d60d31b606082015260800190565b634e487b7160e01b600052603260045260246000fd5b60208082526035908201527f43757272656e742076616c756520646f6573206e6f742066616c6c2077697468604082015274696e207468652065787065637465642072616e676560581b606082015260800190565b81810360008312801583831316838312821617156109db576109db610892565b5092915050565b6000826109ff57634e487b7160e01b600052601260045260246000fd5b500490565b808202811582820484141761021857610218610892565b808201808211156102185761021861089256fea2646970667358221220749741f08a51fe43312321f9fe5def41c5a08c7961659b7ddc87fc57524f719264736f6c634300081a0033";
/// This is hardcoded to be able to have stored txs referencing it.
/// @Pending: include in this repo the tx generating code and use the same constant.
const STORAGE_CONTENTION_SIMULATOR_ADDRESS: Address =
    address!("444111f638c62F3bAFBd5C578688e6282C9710ba");

pub async fn run_backtest_with_synthetic_bundles<ConfigType: LiveBuilderConfig>() -> eyre::Result<()>
{
    // Completely arbitrary. We assume out synthetic contracts and bundles don't care about it.
    let block_number = 1;
    let cli = Cli::parse();

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let fake_block_ref = BlockRef {
        block_number,
        block_timestamp: 0,
    };
    let csv_datasource = CSVDatasource::new(cli.csv)?;
    let available_orders = csv_datasource.get_orders(fake_block_ref).await?;

    println!("Loaded {} orders", available_orders.len());
    let addresses = get_addresses(&available_orders);

    let balances_to_increase = addresses
        .iter()
        .map(|address| (*address, MAX_PAYMENT * 10))
        .collect::<Vec<(Address, u128)>>();
    println!("Available orders: {:?}", available_orders.len());

    let test_context = TestChainState::new_with_balances_and_contracts(
        BlockArgs::default().number(block_number),
        balances_to_increase,
        vec![ContractData::new(
            &Bytes::from_hex(STORAGE_CONTENTION_SIMULATOR_CODE_HEX)?,
            STORAGE_CONTENTION_SIMULATOR_ADDRESS,
        )],
    )?;
    let provider_factory = test_context.provider_factory().clone();
    let sbundle_mergeabe_signers: Vec<Address> = Default::default();
    let ctx = test_context.block_building_context();

    let available_orders: Vec<Order> = available_orders
        .into_iter()
        .map(|order_with_ts| order_with_ts.order)
        .collect();

    let (sim_orders, _) =
        simulate_all_orders_with_sim_tree(provider_factory.clone(), ctx, &available_orders, false)?;

    println!("Simulated orders: {:?}", sim_orders.len());

    let mut order_and_timestamp: HashMap<OrderId, u64> = HashMap::new();
    for order in &sim_orders {
        order_and_timestamp.insert(order.order.id(), 0);
    }

    if cli.show_sim {
        print_simulated_orders(
            &sim_orders,
            &order_and_timestamp,
            fake_block_ref.block_timestamp,
        );
    }

    if !cli.no_block_building {
        let winning_builder = cli
            .builders
            .iter()
            .filter_map(|builder_name: &String| {
                let input = BacktestSimulateBlockInput {
                    ctx: ctx.clone(),
                    builder_name: builder_name.clone(),
                    sbundle_mergeabe_signers: sbundle_mergeabe_signers.clone(),
                    sim_orders: &sim_orders,
                    provider_factory: provider_factory.clone(),
                    cached_reads: None,
                };
                let build_res = config.build_backtest_block(builder_name, input);
                if let Err(err) = &build_res {
                    println!("Error building block: {:?}", err);
                    return None;
                }
                let (block, _) = build_res.ok()?;
                println!("Built block {} with builder: {:?}", cli.block, builder_name);
                println!("Builder profit: {}", format_ether(block.trace.bid_value));
                println!(
                    "Number of used orders: {}",
                    block.trace.included_orders.len()
                );

                println!("Used orders:");
                for order_result in &block.trace.included_orders {
                    println!(
                        "{:>74} gas: {:>8} profit: {}",
                        order_result.order.id().to_string(),
                        order_result.gas_used,
                        format_ether(order_result.coinbase_profit),
                    );
                    if let Order::Bundle(_) | Order::ShareBundle(_) = order_result.order {
                        for tx in &order_result.txs {
                            println!("      ↳ {:?}", tx.hash());
                        }

                        for (to, value) in &order_result.paid_kickbacks {
                            println!(
                                "      - kickback to: {:?} value: {}",
                                to,
                                format_ether(*value)
                            );
                        }
                    }
                }
                Some((builder_name.clone(), block.trace.bid_value))
            })
            .max_by_key(|(_, value)| *value);

        if let Some((builder_name, value)) = winning_builder {
            println!(
                "Winning builder: {} with profit: {}",
                builder_name,
                format_ether(value)
            );
        }
    };

    Ok(())
}

fn get_addresses(orders: &Vec<OrdersWithTimestamp>) -> Vec<Address> {
    let mut addresses = Vec::new();
    for order in orders {
        let txs = order.order.list_txs();
        for (tx, _) in txs {
            addresses.push(tx.signer());
        }
    }
    addresses.dedup();
    addresses
}

/// Convert a timestamp in milliseconds to the slot time relative to the given block timestamp.
fn timestamp_ms_to_slot_time(timestamp_ms: u64, block_timestamp: u64) -> i64 {
    (block_timestamp * 1000) as i64 - (timestamp_ms as i64)
}

/// Print information about simulated orders.
fn print_simulated_orders(
    sim_orders: &[SimulatedOrder],
    order_and_timestamp: &HashMap<OrderId, u64>,
    block_timestamp: u64,
) {
    println!("Simulated orders: ({} total)", sim_orders.len());
    let mut sorted_orders = sim_orders.to_owned();
    sorted_orders.sort_by_key(|order| order.sim_value.coinbase_profit);
    sorted_orders.reverse();
    for order in sorted_orders {
        let order_timestamp = order_and_timestamp
            .get(&order.order.id())
            .copied()
            .unwrap_or_default();

        let slot_time_ms = timestamp_ms_to_slot_time(order_timestamp, block_timestamp);

        println!(
            "{:>74} slot_time_ms: {:>8}, gas: {:>8} profit: {}, parent: {}",
            order.order.id().to_string(),
            slot_time_ms,
            order.sim_value.gas_used,
            format_ether(order.sim_value.coinbase_profit),
            order
                .prev_order
                .map(|prev_order| prev_order.to_string())
                .unwrap_or_else(String::new)
        );
    }
    println!();
}
