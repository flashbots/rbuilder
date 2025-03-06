use alloy_primitives::Address;
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
    /// Run the the supervisor mock
    Supervisor,
    /// Deposit funds to the system
    Deposit {
        #[clap(long, help = "Address to deposit funds to")]
        address: Address,
        #[clap(long, help = "Amount to deposit")]
        amount: u128,
    },
    /// Perform crosschain transaction
    ///
    // struct Identifier {
    //     address origin;      // Account (contract) that emits the log
    //     uint256 blocknumber; // Block number in which the log was emitted
    //     uint256 logIndex;    // Index of the log in the array of all logs emitted in the block
    //    uint256 timestamp;   // Timestamp that the log was emitted
    //     uint256 chainid;     // Chain ID of the chain that emitted the log
    // }
    Cross {
        #[clap(long, help = "Account (contract) that emits the log")]
        origin: Address,
        #[clap(long, help = "Block number in which the log was emitted")]
        blocknumber: u128,
        #[clap(long, help = "Index of the log in the array of all logs emitted in the block")]
        logIndex: u128,
        #[clap(long, help = "Timestamp that the log was emitted")]
        timestamp: u128,
        #[clap(long, help = "Chain ID of the chain that emitted the log")]
        chainid: u128,
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
        Commands::Supervisor => run_supervsior().await,
        Commands::Deposit { address, amount } => {
            let engine_api = EngineApi::new("http://localhost:4444").unwrap();;
            let mut generator = BlockGenerator::new(&engine_api, None, false, 1, None);

            generator.init().await?;

            let block_hash = generator.deposit(address, amount).await?;
            println!("Deposit transaction included in block: {}", block_hash);
            Ok(())
        }
        Commands::Cross {
            origin,
            blocknumber,
            logIndex,
            timestamp,
            chainid,
        } => {
            let engine_api = EngineApi::new("http://localhost:4444").unwrap();;
            let mut generator = BlockGenerator::new(&engine_api, None, false, 1, None);

            generator.init().await?;

            let block_hash = generator.cross(origin, blocknumber, logIndex, timestamp, chainid).await?;
            println!("Cross transaction included in block: {}", block_hash);
            Ok(())
        }
    }
}
