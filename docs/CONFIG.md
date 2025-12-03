Parameters that can come from an env var must be set as "env:XXXX" where XXXX is the name of the var.
Every field has a default if omitted.

## Main fields

| Name | Type | Comments | Default |
|------|------|-------------|---------|
|full_telemetry_server_port|string|             |"0.0.0.0"|
|full_telemetry_server_ip|int      ||             6069|
|redacted_telemetry_server_port|string|             |"0.0.0.0"|
|redacted_telemetry_server_ip|int      ||             6070|
|log_json|bool|JSON vs Raw|false|
|log_level|env/string| Defines the log level (EnvFilter) for each mod. See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/index.html for more info on this.|"info"
|log_color|bool||false|
|error_storage_path|optional string| Path to a sqlite file that will store info for some critical errors|None|
|coinbase_secret_key|optional env/string|If no key is provided a random one is generated. Format is "0x121232432...."|None|
|el_node_ipc_path|optional string| Path for Ipc communication with reth's mempool, Usually something like "/tmp/reth.ipc". If not set mempool will not be used as a source of txs|None|
|jsonrpc_server_port| int| |8645|
|jsonrpc_server_ip|string||"0.0.0.0"|
|jsonrpc_server_max_connections|int|The maximum number of connections|4096|
|ignore_cancellable_orders|bool|If true any order with replacement id will be dropped|true|
|ignore_blobs|bool|If true txs with blobs will be ignored|false|
|chain|string| |"mainnet"|
|reth_datadir|optional string|It will assume default child dirs "db"/"static_files".<br> **It's mandatory to set reth_datadir or reth_db_path+reth_static_files_path or ipc_provider**|"/mnt/data/reth"|
|reth_db_path|optional string|**It's mandatory to set reth_datadir or reth_db_path+reth_static_files_path or ipc_provider**|None|
|reth_static_files_path|optional string|**It's mandatory to set reth_datadir or reth_db_path+reth_static_files_path or ipc_provider**|None|
|ipc_provider|optional | If configured it will use IPC provider for EVM state (instead of accessing a local reth db). More doc pending.|None|
|blocklist_file_path|optional string| Backwards compatibility. Downloads blocklist from a file. Same as setting a file name on blocklist.|None|
|blocklist|optional string| Can contain an url or a file name.<br> If it's a url download blocklist from url and updates periodically.<br>If it's a filename just loads the file (no updates).|None|
|blocklist_url_max_age_hours|optional int|If the downloaded file get older than this we abort.|None|
|blocklist_url_max_age_secs|optional int|If the downloaded file get older than this we abort. Used for debugging only|None|
|require_non_empty_blocklist|bool|if true will not allow to start without a blocklist or with an empty blocklist.|false|
|extra_data|string|Extra data for generated blocks|"extra_data_change_me"|
|sbundle_mergeable_signers|optional vec[string]|mev-share bundles coming from this address are treated in a special way(see [`ShareBundleMerger`])<br>Example:sbundle_mergeable_signers=["0x1234....","0x334344...."]|None|
|sbundle_mergeabe_signers|optional vec[string]|Alias for sbundle_mergeable_signers.Backwards compatible typo soon to be removed. |None|
|simulation_threads|int| Number of threads used for incoming order simulation|1|
|simulation_use_random_coinbase|bool| |true|
|root_hash_use_sparse_trie|bool| Uses cached sparse trie for root hash (much faster)|false|
|root_hash_compare_sparse_trie|bool| If using sparse trie also computes against reth's native version to check the hash is ok|false|
|root_hash_threads| int|Threads used when using reth's native root hash calculation. If 0 global rayon pool is used| 0
| watchdog_timeout_sec| optional int| If now block building is started in this period rbuilder exits.|None|
|live_builders|vec[string]| List of `builders` to be used for live building.<br>Notice that you can define on **builders** some builders and select only a few here.|["mgp-ordering","mp-ordering"]|
|evm_caching_enable|bool|If enabled per block EVM execution will be enabled|false|
|faster_finalize|bool| If enabled improves block finalization by catching proofs|false|
|time_to_keep_mempool_txs_secs|u64| /// After this time a mempool tx is dropped.|1|
|backtest_fetch_mempool_data_dir|env/string|Dir used to store mempool data used in backtesting|"/mnt/data/mempool"|
|backtest_fetch_eth_rpc_url|string|url to EL node RPC used in backtesting|"http://127.0.0.1:8545"|
|backtest_fetch_eth_rpc_parallel| int|Number of parallel connections allowed on backtest_fetch_eth_rpc_url|1|
|backtest_fetch_output_file|string | Path to a sqlite containing block information for backtesting. This file is filled with backtest-fetch|"/tmp/rbuilder-backtest.sqlite"|
|backtest_results_store_path|string|Path to a sqlite containing backtest results|"/tmp/rbuilder-backtest-results.sqlite"|
|backtest_builders|vec[string]| List of `builders` to be used for backtesting.<br>Notice that you can define on **builders** some builders and select only a few here.|[]|
|backtest_protect_bundle_signers|vec[string]|Doc pending.|[]|
|orderflow_tracing_store_path|Optional string|We will store a file per block in this path.|None|
|orderflow_tracing_max_blocks|Optional int|Max number of blocks to keep in disk.<br>Remember to also set this if you enable orderflow_tracing_store_path.|None|

## L1 related fields
| Name | Type | Comments | Default |
|------|------|-------------|---------|
|relays|vec[RelayConfig]| List of relays used to get validator registration info and/or submitting. Below are the details for RelayConfig fields. Example: <br>[[relays]]<br>name = "relay1"<br>optimistic = true<br>priority = 1<br>url = "https://relay1"<br>use_gzip_for_submit = true<br>use_ssz_for_submit = true<br>mode:full<br><br>[[relays]]<br>name = "relay2"<br>...more params...|[]|
|registration_update_interval_ms|optional u64| Period used to refresh validators registration info.|5000|
|RelayConfig.name|mandatory string| Human readable name for the relay||
|RelayConfig.url|mandatory string| Url to relay's endpoint||
|RelayConfig.grpc_url|optional string| Url to relay's gRPC endpoint (only bloxroute at 2025/08/20).|None|
|RelayConfig.authorization_header|optional env/string|If set "authorization" header will be added to RPC calls|None|
|RelayConfig.builder_id_header|optional env/string|If set "X-Builder-Id" header will be added to RPC calls|None|
|RelayConfig.api_token_header|optional env/string|If set "X-Api-Token" header will be added to RPC calls|None|
|RelayConfig.mode| string| Valid values:<br>"full": Relay will be used to get validator registration info and for submitting blocks<br>"slot_info": Relay will be used to get validator registration info<br>"test": Relay will be used for submitting blocks and extra metadata will be added|"full"|
|RelayConfig.use_ssz_for_submit|optional bool||false|
|RelayConfig.use_gzip_for_submit|optional bool||false|
|RelayConfig.optimistic|optional bool||false|
|RelayConfig.interval_between_submissions_ms|optional int| Caps the submission rate to the relay|None|
|RelayConfig.max_bid_eth|optional string| Max bid we can submit to this relay. Any bid above this will be skipped.<br>None -> No limit.|None|
|RelayConfig.is_bloxroute|bool|Set to `true` for bloxroute relays to add extra headers.|false|
|RelayConfig.ask_for_filtering_validators|optional bool| Adds "filtering=true" as query to the call relay/v1/builder/validators to get all validators (including those filtering OFAC).<br>On 2025/06/24 only supported by ultrasound.|false|
|RelayConfig.can_ignore_gas_limit|optional bool| If we submit a block with a different gas than the one the validator registered with in this relay the relay does not mind. Useful for gas limit conflicts. On 2025/08/20 only ultrasound confirmed that is ok with this. (we didn't asked the rest yet)|false|
|enabled_relays| vec["string"]| Extra hardcoded relays to add (see DEFAULT_RELAYS in [config.rs](../crates/rbuilder/src/live_builder/config.rs))|[]|
|relay_secret_key|optional env/string|Secret key that will be used to sign normal submissions to the relay.|None|
|optimistic_relay_secret_key|optional env/string|Secret key that will be used to sign optimistic submissions to the relay.|None|
|optimistic_enabled|bool|When enabled builder will make optimistic submissions to optimistic relays|false|
|optimistic_max_bid_value_eth|string| Bids above this value will always be submitted in non-optimistic mode.|"0.0"|
|cl_node_url|vec[env/stirng]| Array if urls to CL clients to get the new payload events|["http://127.0.0.1:3500"]
|genesis_fork_version|optional string|Genesis fork version for the chain. If not provided it will be fetched from the beacon client.|None|
|relay_bid_scrapers||See [bid scraper publishers](../crates/bid-scraper/README.md) |Empty|
|max_order_execution_duration_warning_us|optional u64| If set, while building a block if an order execution duration is greater than this, we will log a warning with some info about the order.|None|
## Building algorithms
rbuilder can multiple building algorithms and each algorithm can be instantiated multiple times with it's own set of parameters each time.
Each instantiated algorithm starts with:
| Name | Type | Comments | Default |
|------|------|-------------|---------|
|name|mandatory string|Name of the instance. Referenced on live_builders/backtest_builders||
|algo|mandatory string| Algorithm to use. Currently we have 2 algorithms:<br>- "ordering-builder": Uses OrderingBuildingAlgorithm<br>- "parallel-builder": (Experimental) Uses ParallelBuilder.

### Fields for algo="ordering-builder"
| Name | Type | Comments | Default |
|------|------|-------------|---------|
|discard_txs|mandatory bool| If a tx inside a bundle or sbundle fails with TransactionErr (don't confuse this with reverting which is TransactionOk with !.receipt.success) and it's configured as allowed to revert (for bundles tx in reverting_tx_hashes or dropping_tx_hashes, for sbundles: TxRevertBehavior != NotAllowed) we continue the  execution of the bundle/sbundle. The most typical value is true.||
|sorting|mandatory string|Valid values:<br>-"mev-gas-price": Sorts the SimulatedOrders by its effective gas price. This not only includes the explicit gas price set in the tx but also the direct coinbase payments so we compute it as (coinbase balance delta after executing the order) / (gas used).<br>-"max-profit": Sorts the SimulatedOrders by its absolute profit which is computed as the coinbase balance delta after executing the order.<br>-"type-max-profit": (Experimental) Orders are ordered by their origin (bundle/sbundles then mempool) and then by their absolute profit.<br>-"length-three-max-profit":(Experimental) Orders are ordered by length 3 (orders length >= 3 first) and then by their absolute profit.<br>-"length-three-mev-gas-price":(Experimental) Orders are ordered by length 3 (orders length >= 3 first) and then by their mev gas price.||
|failed_order_retries|mandatory int | Only when a tx fails because the profit was worst than expected: Number of time an order can fail during a single block building iteration.<br> When thi happens it gets reinserted in the PrioritizedOrderStore with the new simulated profit (the one that failed).||
|drop_failed_orders|mandatory bool| if a tx fails in a block building iteration it's dropped so next iterations will not use it.||
|build_duration_deadline_ms|optional int| Amount of time allocated for EVM execution while building block. If None it only stops when it tried all orders.| None|
|pre_filtered_build_duration_deadline_ms|optional int| Amount of time allocated for EVM execution for the pre-filtered building step. If None it only stops when it tried all orders.<br>In this second building step the building algorithm will only try to include orders other algorithms landed in their last locks.|0|
|ignore_mempool_profit_on_bundles|bool|When computing profit to prioritize orders on s/bundles any profit from a mempool tx will be ignored.|false|

### Fields for algo="parallel-builder"


| Name | Type | Comments | Default |
|------|------|-------------|---------|
|discard_txs|mandatory bool| If a tx inside a bundle or sbundle fails with TransactionErr (don't confuse this with reverting which is TransactionOk with !.receipt.success) and it's configured as allowed to revert (for bundles tx in reverting_tx_hashes or dropping_tx_hashes, for sbundles: TxRevertBehavior != NotAllowed) we continue the  execution of the bundle/sbundle. The most typical value is true.||
|num_threads| mandatory int| Number of threads to use for merging.||

## Bidding fields
| Name | Type | Comments | Default |
|------|------|-------------|---------|
|slot_delta_to_start_bidding_ms| optional int| When the sample bidder (see TrueBlockValueBiddingService) will start bidding relative to the slot start.<br>Usually a negative number.|None|
|subsidy|optional string|Value added to the bids (see TrueBlockValueBiddingService).<br>The builder address must have enough balance for the subsidy.<br>Example:"1.23" for 1.23 ETH|None|

    
