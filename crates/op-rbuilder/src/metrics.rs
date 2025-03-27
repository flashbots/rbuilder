use reth_metrics::{metrics::Counter, metrics::Gauge, metrics::Histogram, Metrics};

/// op-rbuilder metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "op_rbuilder")]
pub struct OpRBuilderMetrics {
    /// Builder balance of the last block
    pub builder_balance: Gauge,
    /// Number of builder landed blocks
    pub builder_landed_blocks: Gauge,
    /// Last built block height
    pub last_landed_block_height: Gauge,
    /// Number of blocks the builder did not land
    pub builder_landed_blocks_missed: Gauge,
    /// Block built success
    pub block_built_success: Counter,
    /// Total duration of building a block
    pub total_block_built_duration: Histogram,
    /// Duration of fetching transactions from the pool
    pub transaction_pool_fetch_duration: Histogram,
    /// Duration of sequencer transaction execution
    pub sequencer_tx_duration: Histogram,
    /// Duration of state merge transitions
    pub state_transition_merge_duration: Histogram,
    /// Duration of payload simulation of all transactions
    pub payload_tx_simulation_duration: Histogram,
    /// Number of transaction considered for inclusion in the block
    pub payload_num_tx_considered: Histogram,
    /// Payload byte size
    pub payload_byte_size: Histogram,
    /// Number of transactions in the payload
    pub payload_num_tx: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success: Histogram,
    /// Number of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail: Histogram,
    /// Duration of tx simulation
    pub tx_simulation_duration: Histogram,
    /// Byte size of transactions
    pub tx_byte_size: Histogram,
    /// Number of reverted transactions
    pub num_reverted_tx: Counter,
    /// Number of cross-chain transactions
    pub num_cross_chain_tx: Counter,
    /// Number of cross-chain transactions that didn't pass supervisor validation
    pub num_cross_chain_tx_fail: Counter,
    /// Number of cross-chain transactions that weren't verified because of the timeout
    pub num_cross_chain_tx_timeout: Counter,
    /// Number of cross-chain transactions that weren't verified because of the server error
    pub num_cross_chain_tx_server_error: Counter,
}

impl OpRBuilderMetrics {
    pub fn inc_num_reverted_tx(&self, num_reverted_tx: usize) {
        self.num_reverted_tx.increment(num_reverted_tx as u64);
    }

    pub fn inc_builder_landed_blocks(&self) {
        self.builder_landed_blocks.increment(1);
    }

    pub fn dec_builder_landed_blocks(&self) {
        self.builder_landed_blocks.decrement(1);
    }

    pub fn inc_builder_landed_blocks_missed(&self) {
        self.builder_landed_blocks_missed.increment(1);
    }

    pub fn set_last_landed_block_height(&self, height: u64) {
        self.last_landed_block_height.set(height as f64);
    }

    pub fn set_builder_balance(&self, balance: f64) {
        self.builder_balance.set(balance);
    }

    pub fn inc_num_cross_chain_tx_fail(&self) {
        self.num_cross_chain_tx_fail.increment(1);
    }

    pub fn inc_num_cross_chain_tx(&self) {
        self.num_cross_chain_tx.increment(1);
    }

    pub fn inc_num_cross_chain_tx_timeout(&self) {
        self.num_cross_chain_tx_timeout.increment(1);
    }

    pub fn inc_num_cross_chain_tx_server_error(&self) {
        self.num_cross_chain_tx_server_error.increment(1);
    }
}

/// Transaction pool metrics
#[derive(Metrics)]
#[metrics(scope = "payloads")]
pub(crate) struct PayloadBuilderMetrics {
    /// Total number of times an empty payload was returned because a built one was not ready.
    pub(crate) requested_empty_payload: Counter,
    /// Total number of initiated payload build attempts.
    pub(crate) initiated_payload_builds: Counter,
    /// Total number of failed payload build attempts.
    pub(crate) failed_payload_builds: Counter,
}

impl PayloadBuilderMetrics {
    pub(crate) fn inc_requested_empty_payload(&self) {
        self.requested_empty_payload.increment(1);
    }

    pub(crate) fn inc_initiated_payload_builds(&self) {
        self.initiated_payload_builds.increment(1);
    }

    pub(crate) fn inc_failed_payload_builds(&self) {
        self.failed_payload_builds.increment(1);
    }
}
