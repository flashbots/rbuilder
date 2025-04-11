use std::hash::Hash;
use std::sync::Arc;

use ScheduledJob::CronjobAdded;
use alloy_consensus::{Block, TxEip1559};
use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, Uint, address};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::abi::EMPTY_BYTES;
use alloy_sol_types::{SolEventInterface, sol};
use alloy_transport_http::reqwest;
use futures_util::{Stream, StreamExt};
use op_alloy_consensus::OpTypedTransaction;
use reth::builder::NodeAdapter;
use reth::builder::components::Components;
use reth::rpc::eth::EthApiServer;
use reth_db::DatabaseEnv;
use reth_evm::Evm;
use reth_node_api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter};
use reth_node_ethereum::BasicBlockExecutorProvider;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::{OpNetworkPrimitives, OpNode};
use reth_optimism_primitives::{OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::OpEthApi;
use reth_optimism_txpool::{OpPooledTransaction, OpTransactionValidator};
use reth_primitives::{EthPrimitives, Log, TransactionSigned};
use reth_primitives::{Recovered, RecoveredBlock};
use reth_primitives_traits::transaction::signed::SignedTransaction;
use reth_provider::providers::BlockchainProvider;
use reth_provider::{CanonStateNotification, Chain, ExecutionOutcome};
use reth_transaction_pool::blobstore::DiskFileBlobStore;
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolTransaction, TransactionOrigin, TransactionPool,
    TransactionValidationTaskExecutor,
};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::info;

sol!(ScheduledJob, "cronjob_abi.json");
use crate::tx_signer::Signer;

use self::ScheduledJob::ScheduledJobEvents;

const SLOT_TIME: u64 = 2; // 2 seconds
const CRONJOB_CONTRACT_ADDRESS: Address = address!("0x5FbDB2315678afecb367f032d93F642f64180aa3");
type OpPool = Pool<
    TransactionValidationTaskExecutor<
        OpTransactionValidator<
            BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>,
            OpPooledTransaction,
        >,
    >,
    CoinbaseTipOrdering<OpPooledTransaction>,
    DiskFileBlobStore,
>;
type EthApi = OpEthApi<
    NodeAdapter<
        FullNodeTypesAdapter<
            OpNode,
            Arc<DatabaseEnv>,
            BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>,
        >,
        Components<
            FullNodeTypesAdapter<
                OpNode,
                Arc<DatabaseEnv>,
                BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>,
            >,
            OpNetworkPrimitives,
            Pool<
                TransactionValidationTaskExecutor<
                    OpTransactionValidator<
                        BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>,
                        OpPooledTransaction,
                    >,
                >,
                CoinbaseTipOrdering<OpPooledTransaction>,
                DiskFileBlobStore,
            >,
            OpEvmConfig,
            BasicBlockExecutorProvider<OpEvmConfig>,
            Arc<OpBeaconConsensus<OpChainSpec>>,
        >,
    >,
>;

pub struct CronJob {
    scheduled_transactions: HashMap<TxHash, ScheduledJobEvents>,
    pool: OpPool,
    eth_api: EthApi,
    signer: Signer,
    chain_id: u64,
}

impl CronJob {
    pub fn new(signer: Signer, pool: OpPool, eth_api: EthApi, chain_id: u64) -> Self {
        Self {
            scheduled_transactions: HashMap::default(),
            pool,
            eth_api,
            signer,
            chain_id,
        }
    }

    pub async fn run_with_stream<St>(mut self, mut events: St) -> eyre::Result<()>
    where
        St: Stream<Item = CanonStateNotification<OpPrimitives>> + Unpin + 'static,
    {
        while let Some(event) = events.next().await {
            if let Some(reverted) = event.reverted() {
                self.revert(&reverted).await?;
            }

            let committed = event.committed();
            self.commit(&committed).await?;
        }

        Ok(())
    }

    /// Process a chain commit.
    ///
    /// This function decodes all transactions in the block, updates the metrics for builder built blocks
    async fn commit(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain commit");
        let events = decode_chain_into_events(chain);
        for (tx, event) in events {
            println!("found cron job event transaction: {:?}", tx);
            self.scheduled_transactions.insert(*tx.tx_hash(), event);
        }
        self.process_scheduled_transactions(chain.tip().timestamp, chain.tip().base_fee_per_gas)
            .await?;

        Ok(())
    }

    /// Process a chain revert.
    ///
    /// This function decodes all transactions in the block, updates the metrics for builder built blocks
    async fn revert(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain revert");
        let events = decode_chain_into_events(chain);
        for (tx, _) in events {
            self.scheduled_transactions.remove(tx.tx_hash());
        }

        Ok(())
    }

    async fn process_scheduled_transactions(&mut self, block_time: u64, base_fee: Option<u64>) -> eyre::Result<()> {
        let nonce = self
            .eth_api
            .transaction_count(self.signer.address, None)
            .await?;
        let mut nonce: u64 = nonce.try_into()?;
        println!("nonce: {:?}", nonce);
        let mut hashes_to_remove = Vec::new();
        for (tx_hash, event) in self.scheduled_transactions.iter() {
            match event {
                ScheduledJobEvents::CronjobAdded(CronjobAdded { cronjob }) => {
                    let start_time: u64 = cronjob.startTime.try_into()?;
                    let end_time: u64 = cronjob.endTime.try_into()?;
                    let check = cronjob.check.functionCalldata.clone();
                    let contract_address = cronjob.check.recipient;

                    let request = TransactionRequest {
                        to: Some(TxKind::Call(contract_address)),
                        input: TransactionInput {
                            input: None,
                            data: Some(check),
                        },
                        ..Default::default()
                    };
                    let predicate = self.eth_api.call(request, None, None, None).await?;
                    if (block_time + SLOT_TIME) >= start_time
                        && (block_time + SLOT_TIME) <= end_time
                        && predicate != Bytes::from(vec![0; 32])
                    {
                        for function in cronjob.functionCalls.clone() {
                            // create transaction
                            let tx = OpTypedTransaction::Eip1559(TxEip1559 {
                                chain_id: self.chain_id,
                                nonce,
                                gas_limit: 10000000,
                                max_fee_per_gas: base_fee.unwrap_or(0).into(),
                                max_priority_fee_per_gas: 0,
                                to: TxKind::Call(function.recipient),
                                input: function.functionCalldata,
                                ..Default::default()
                            });
                            let tx = self.signer.sign_tx(tx)?;
                            let tx_bytes: Bytes = tx.encoded_2718().into();
                            let result = self.eth_api.send_raw_transaction(tx_bytes).await;
                            println!("tx_hash: {:?}", tx_hash);
                            println!("result: {:?}", result);
                            nonce += 1;
                        }
                    } else if block_time >= end_time {
                        hashes_to_remove.push(*tx_hash);
                    }
                }
                _ => {}
            }
        }

        // for hash in hashes_to_remove {
        //     self.scheduled_transactions.remove(&hash);
        // }

        Ok(())
    }

    async fn send_to_rpc(&self, transaction: Recovered<OpTransactionSigned>) -> eyre::Result<()> {
        let client = reqwest::Client::new();

        // Convert the transaction to bytes and hex encode it
        // let tx_bytes = transaction;
        let tx_hex = format!("0x{}", "");

        // Create the JSON-RPC request
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "eth_sendRawTransaction",
            "params": [tx_hex],
            "id": 1
        });

        // Send the request
        let response = client
            .post("http://localhost:8545")
            .json(&payload)
            .send()
            .await?
            .json::<Value>()
            .await?;

        // Check for errors in the response
        if response.get("error").is_some() {
            let error = response["error"].to_string();
            return Err(eyre::eyre!("RPC error: {}", error));
        }

        info!("Transaction sent: {}", response["result"]);
        Ok(())
    }
}

/// Decode chain of blocks into a flattened list of receipt logs, and filter only
/// [L1StandardBridgeEvents].
fn decode_chain_into_events(
    chain: &Chain<OpPrimitives>,
) -> impl Iterator<Item = (&OpTransactionSigned, ScheduledJobEvents)> {
    chain
        // Get all blocks and receipts
        .blocks_and_receipts()
        // Get all receipts
        .flat_map(|(block, receipts)| {
            block
                .body()
                .transactions
                .iter()
                .zip(receipts.iter())
                .map(move |(tx, receipt)| (tx, receipt))
        })
        // Get all logs from expected bridge contracts
        .flat_map(|(tx, receipt)| {
            receipt
                .as_receipt()
                .logs
                .iter()
                .filter(|log| CRONJOB_CONTRACT_ADDRESS == log.address)
                .map(move |log| (tx, log))
        })
        // Decode and filter bridge events
        .filter_map(|(tx, log)| {
            println!("log: {:?}", log);
            ScheduledJobEvents::decode_raw_log(log.topics(), &log.data.data, true)
                .ok()
                .map(|event| (tx, event))
        })
}
