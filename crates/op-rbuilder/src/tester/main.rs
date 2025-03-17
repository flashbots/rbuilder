use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use clap::Parser;
use op_rbuilder::tester::*;

/// CLI Commands
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    /// Generate genesis configuration
    Genesis {
        #[clap(long, help = "Output path for genesis files")]
        output: Option<String>,
    },
    /// Run the testing system
    Run {
        #[clap(long, short, action)]
        validation: bool,

        #[clap(long, short, action, default_value = "false")]
        no_tx_pool: bool,

        #[clap(long, short, action, default_value = "1")]
        block_time_secs: u64,

        #[clap(long, short, action)]
        flashblocks_endpoint: Option<String>,
    },
    Crash {},
    /// Deposit funds to the system
    Deposit {
        #[clap(long, help = "Address to deposit funds to")]
        address: Address,
        #[clap(long, help = "Amount to deposit")]
        amount: u128,
    },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Genesis { output } => generate_genesis(output).await,
        Commands::Run {
            validation,
            no_tx_pool,
            block_time_secs,
            flashblocks_endpoint,
        } => {
            run_system(
                validation,
                no_tx_pool,
                block_time_secs,
                flashblocks_endpoint,
            )
            .await
        }
        Commands::Crash {} => {
            let engine_api = EngineApi::new("http://localhost:4444").unwrap();
            let block = engine_api.latest().await?.unwrap();

            // fork two blocks behind
            let fork_header = engine_api
                .get_block_by_number(BlockNumberOrTag::Number(block.header.number - 1), false)
                .await?
                .unwrap()
                .header;

            println!(
                "fork header {:?} {:?}",
                fork_header.parent_hash, fork_header.hash
            );

            let fork_choice_updated = engine_api
                .update_forkchoice(fork_header.parent_hash, fork_header.hash, None)
                .await?;

            println!("fork choice updated {:?}", fork_choice_updated);
            Ok(())
        }
        Commands::Deposit { address, amount } => {
            let engine_api = EngineApi::builder().build().unwrap();
            let mut generator = BlockGenerator::new(&engine_api, None, false, 1, None);

            generator.init().await?;

            let block_hash = generator.deposit(address, amount).await?;
            println!("Deposit transaction included in block: {}", block_hash);
            Ok(())
        }
    }
}
