// main.rs

use serde::Deserialize;
use std::error::Error;
use std::fs;
use std::time::Duration;

// Define the main Config struct with all the fields from your TOML configuration
#[derive(Deserialize)]
struct Config {
    // General settings
    log_json: bool,
    log_level: String,
    redacted_telemetry_server_port: u16,
    redacted_telemetry_server_ip: String,
    full_telemetry_server_port: u16,
    full_telemetry_server_ip: String,
    chain: String,
    reth_datadir: String,

    coinbase_secret_key: String,
    relay_secret_key: String,
    optimistic_relay_secret_key: String,

    cl_node_url: Vec<String>,
    jsonrpc_server_port: u16,
    jsonrpc_server_ip: String,
    el_node_ipc_path: String,
    extra_data: String,

    blocklist_file_path: String,

    dry_run: bool,
    dry_run_validation_url: String,

    ignore_cancellable_orders: bool,

    max_concurrent_seals: u32,

    sbundle_mergeabe_signers: Vec<String>,

    live_builders: Vec<String>,

    relays: Vec<RelayConfig>,
    builders: Vec<BuilderConfig>,
}

// Relay configuration struct
#[derive(Deserialize)]
struct RelayConfig {
    name: String,
    url: String,
    priority: u32,
    use_ssz_for_submit: bool,
    use_gzip_for_submit: bool,
}

// Builder configuration struct
#[derive(Deserialize)]
struct BuilderConfig {
    name: String,
    algo: String,
    discard_txs: bool,
    sorting: Option<String>,
    failed_order_retries: Option<u32>,
    drop_failed_orders: Option<bool>,
    num_threads: Option<u32>,
    merge_wait_time_ms: Option<u64>,
    duration_ms: Option<u64>, // New field for durations
}

// Implement default values for BuilderConfig
impl BuilderConfig {
    fn set_defaults(&mut self) {
        if self.failed_order_retries.is_none() {
            self.failed_order_retries = Some(1);
        }
        if self.drop_failed_orders.is_none() {
            self.drop_failed_orders = Some(true);
        }
        if self.duration_ms.is_none() {
            self.duration_ms = Some(match self.name.as_str() {
                "mp-ordering" => 100,
                "mgp-ordering" => 200,
                "merging" => 150,
                _ => 100, // Default duration
            });
        }
    }
}

// Function to load configuration
fn load_config() -> Result<Config, Box<dyn Error>> {
    // Read the TOML configuration file
    let config_content = fs::read_to_string("config-live-example.toml")?;
    let mut config: Config = toml::from_str(&config_content)?;

    // Set default duration values if not specified
    for builder in &mut config.builders {
        builder.set_defaults();
    }

    Ok(config)
}

// Trait for builders
trait Builder {
    fn build(&self);
}

// Implementations for your builders

struct OrderingBuilder {
    name: String,
    duration: Duration,
    discard_txs: bool,
    sorting: Option<String>,
    failed_order_retries: u32,
    drop_failed_orders: bool,
    // ... other fields if any
}

impl OrderingBuilder {
    fn new(config: &BuilderConfig) -> Self {
        Self {
            name: config.name.clone(),
            duration: Duration::from_millis(config.duration_ms.unwrap()),
            discard_txs: config.discard_txs,
            sorting: config.sorting.clone(),
            failed_order_retries: config.failed_order_retries.unwrap_or(1),
            drop_failed_orders: config.drop_failed_orders.unwrap_or(true),
            // ... initialize other fields based on config
        }
    }
}

impl Builder for OrderingBuilder {
    fn build(&self) {
        println!(
            "Building with OrderingBuilder: {}, duration: {:?}, discard_txs: {}, sorting: {:?}, failed_order_retries: {}, drop_failed_orders: {}",
            self.name,
            self.duration,
            self.discard_txs,
            self.sorting,
            self.failed_order_retries,
            self.drop_failed_orders
        );
        // Use self.duration in your build logic
        // Simulate build process
        std::thread::sleep(self.duration);
        // ... rest of the build logic
    }
}

struct MergingBuilder {
    name: String,
    duration: Duration,
    discard_txs: bool,
    num_threads: u32,
    merge_wait_time_ms: u64,
    // ... other fields if any
}

impl MergingBuilder {
    fn new(config: &BuilderConfig) -> Self {
        Self {
            name: config.name.clone(),
            duration: Duration::from_millis(config.duration_ms.unwrap()),
            discard_txs: config.discard_txs,
            num_threads: config.num_threads.unwrap_or(1),
            merge_wait_time_ms: config.merge_wait_time_ms.unwrap_or(100),
            // ... initialize other fields based on config
        }
    }
}

impl Builder for MergingBuilder {
    fn build(&self) {
        println!(
            "Building with MergingBuilder: {}, duration: {:?}, discard_txs: {}, num_threads: {}, merge_wait_time_ms: {}",
            self.name,
            self.duration,
            self.discard_txs,
            self.num_threads,
            self.merge_wait_time_ms
        );
        // Use self.duration in your build logic
        // Simulate build process
        std::thread::sleep(self.duration);
        // ... rest of the build logic
    }
}

// Function to initialize builders
fn initialize_builders(builders_config: &[BuilderConfig]) -> Vec<Box<dyn Builder>> {
    let mut builders: Vec<Box<dyn Builder>> = Vec::new();

    for config in builders_config {
        let builder: Box<dyn Builder> = match config.algo.as_str() {
            "ordering-builder" => Box::new(OrderingBuilder::new(config)),
            "merging-builder" => Box::new(MergingBuilder::new(config)),
            _ => {
                println!("Unknown builder algorithm: {}", config.algo);
                continue;
            }
        };
        builders.push(builder);
    }

    builders
}

fn main() -> Result<(), Box<dyn Error>> {
    // Load the configuration
    let config = load_config()?;

    // Initialize the builders
    let builders = initialize_builders(&config.builders);

    // Use the builders
    for builder in builders {
        builder.build();
    }

    Ok(())
}
