use alloy_consensus::{
    EthereumTxEnvelope, EthereumTypedTransaction, SignableTransaction as _, TxEip1559, TxEip4844,
};
use alloy_eips::{eip1559::BaseFeeParams, BlockId, Encodable2718};
use alloy_primitives::{
    hex,
    map::{AddressMap, AddressSet, HashMap},
    Address, Bytes, TxKind, B256, U256,
};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{AccountInfo, Header};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt as _};
use reqwest::Client;
use serde_json::json;
use std::{cell::OnceCell, time::Duration};
use tokio::time;
use tracing::*;

use crate::config::{RebalancerAccount, RebalancerRule};

pub struct Rebalancer<P> {
    provider: P,
    builder_client: reqwest::Client,
    builder_url: String,
    transfer_max_priority_fee_per_gas: u128,
    accounts: HashMap<String, RebalancerAccount<PrivateKeySigner>>,
    rules: Vec<RebalancerRule>,
    tracked_accounts: OnceCell<AddressSet>,
    timeout: Duration,
    retry_delay: Duration,
}

impl<P: Provider> Rebalancer<P> {
    pub fn new(
        provider: P,
        builder_url: String,
        transfer_max_priority_fee_per_gas: u128,
        accounts: HashMap<String, RebalancerAccount<PrivateKeySigner>>,
        rules: Vec<RebalancerRule>,
        timeout: Duration,
        retry_delay: Duration,
    ) -> Self {
        let builder_client = reqwest::Client::builder().timeout(timeout).build().unwrap();
        Self {
            provider,
            builder_client,
            builder_url,
            transfer_max_priority_fee_per_gas,
            accounts,
            rules,
            timeout,
            retry_delay,
            tracked_accounts: OnceCell::new(),
        }
    }

    fn tracked_accounts(&self) -> &AddressSet {
        self.tracked_accounts.get_or_init(|| {
            AddressSet::from_iter(
                self.accounts
                    .values()
                    .map(|acc| acc.secret.address())
                    .chain(self.rules.iter().map(|rule| rule.destination)),
            )
        })
    }

    async fn fetch_accounts(&self, block_hash: B256) -> eyre::Result<AddressMap<AccountInfo>> {
        let block_id = BlockId::hash(block_hash);

        let futs = FuturesUnordered::default();
        for target in self.tracked_accounts() {
            futs.push(async move {
                (
                    *target,
                    time::timeout(
                        self.timeout,
                        self.provider.get_account_info(*target).block_id(block_id),
                    )
                    .await,
                )
            });
        }

        let mut infos = AddressMap::default();
        for (target, result) in futs.collect::<Vec<_>>().await {
            match result {
                Ok(Ok(info)) => {
                    debug!(target: "rebalancer", %target, ?info, %block_hash, "Fetched account info for account");
                    infos.insert(target, info);
                }
                // We don't want to rebalance if any of the account info fetches failed.
                Ok(Err(error)) => {
                    return Err(eyre::eyre!(
                        "error fetching account info for {target}: {error}"
                    ));
                }
                Err(_) => return Err(eyre::eyre!("timed out fetching account info for {target}")),
            }
        }

        Ok(infos)
    }

    async fn on_new_block(&self, header: Header) -> eyre::Result<()> {
        info!(target: "rebalancer", number = header.number, hash = %header.hash, "Received new block");
        let accounts = self.fetch_accounts(header.hash).await?;
        debug!(target: "rebalancer", number = header.number, hash = %header.hash, accounts = accounts.len(), "Updated account infos for tracked accounts");
        let mut transfers_by_source = HashMap::<String, Vec<Transfer>>::default();
        for rule in &self.rules {
            let destination_balance = accounts
                .get(&rule.destination)
                .ok_or(eyre::eyre!("missing account for {}", rule.destination))?
                .balance;

            if destination_balance > rule.destination_min_balance {
                trace!(target: "rebalancer", number = header.number, hash = %header.hash, %rule.description, %rule.destination, %rule.destination_min_balance, %destination_balance, "Rebalancing destination balance above minimum");
                continue;
            }

            let destination_target_delta = rule
                .destination_target_balance
                .checked_sub(destination_balance)
                .expect("misconfiguration");

            let transfer = Transfer {
                destination: rule.destination,
                amount: destination_target_delta,
                description: rule.description.clone(),
            };
            transfers_by_source
                .entry(rule.source_id.clone())
                .or_default()
                .push(transfer);
        }

        for (source_id, transfers) in transfers_by_source {
            let total_amount_out = transfers.iter().map(|t| t.amount).sum();

            let source = self
                .accounts
                .get(&source_id)
                .ok_or(eyre::eyre!("missing source {source_id}"))?;
            let source_address = source.secret.address();
            let source_account = accounts
                .get(&source_address)
                .ok_or(eyre::eyre!("missing account {source_address}"))?;
            let source_balance = source_account.balance;

            if source_balance
                .checked_sub(total_amount_out)
                .is_none_or(|final_balance| final_balance < source.min_balance)
            {
                let rules = transfers
                    .into_iter()
                    .map(|t| t.description)
                    .collect::<Vec<_>>();
                warn!(target: "rebalancer", number = header.number, hash = %header.hash, %source_id, %source_address, %source_balance, %total_amount_out, ?rules, "Source account balance too low");
                continue;
            }

            let client = self.builder_client.clone();
            let builder_url = self.builder_url.clone();
            let head = header.clone();
            let signer = source.secret.clone();
            let nonce = source_account.nonce;
            let max_priority_fee_per_gas = self.transfer_max_priority_fee_per_gas;
            tokio::spawn(async move {
                if let Err(error) = send_system_transactions(
                    client,
                    &builder_url,
                    head.clone(),
                    signer,
                    nonce,
                    max_priority_fee_per_gas,
                    transfers,
                )
                .await
                {
                    error!(target: "rebalancer", number = head.number, hash = %head.hash, %source_id, %source_address, ?error, "Error sending system bundle");
                }
            });
        }

        Ok(())
    }

    pub async fn run(self) -> eyre::Result<()> {
        loop {
            let mut subscription = self.provider.subscribe_blocks().await?.into_stream();
            while let Some(header) = subscription.next().await {
                if let Err(error) = self.on_new_block(header.clone()).await {
                    error!(target: "rebalancer", number = header.number, hash = %header.hash, ?error, "Error handling block");
                }
            }
            warn!(target: "rebalancer", delay = ?self.retry_delay, "New block subscription has been terminated. Retrying...");
            tokio::time::sleep(self.retry_delay).await;
        }
    }
}

async fn send_system_transactions(
    client: Client,
    builder_url: &str,
    head: Header,
    signer: PrivateKeySigner,
    nonce: u64,
    max_priority_fee_per_gas: u128,
    transfers: Vec<Transfer>,
) -> eyre::Result<()> {
    let signer_address = signer.address();
    let base_fee = head
        .next_block_base_fee(BaseFeeParams::ethereum())
        .unwrap_or_default();
    let mut futs = FuturesUnordered::default();
    let mut next_nonce = nonce;
    for transfer in transfers {
        // Prepare transaction
        let max_fee_per_gas = base_fee as u128 + max_priority_fee_per_gas;
        let transaction = EthereumTypedTransaction::<TxEip4844>::Eip1559(TxEip1559 {
            chain_id: 1,
            nonce: next_nonce,
            to: TxKind::Call(transfer.destination),
            value: U256::from(transfer.amount),
            gas_limit: 21_000,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            access_list: Default::default(),
            input: Bytes::default(),
        });
        let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
        let signed = EthereumTxEnvelope::<TxEip4844>::new_unhashed(transaction, signature);

        // Send the request to the builder
        let client = client.clone();
        let tx_hash = *signed.hash();
        info!(target: "rebalancer", head_number = head.number, %head.hash, %signer_address, rule = %transfer.description, "Sending system transaction");
        futs.push(
            async move {
                let encoded = hex::encode_prefixed(signed.encoded_2718());
                let request = json!({
                    "id": 1,
                    "jsonrpc": "2.0",
                    "method": "eth_sendRawTransaction",
                    "params": [encoded],
                });
                let response = client.post(builder_url).json(&request).send().await?;
                Ok::<_, eyre::Error>(response)
            }
            .map(move |result| (tx_hash, transfer, result)),
        );

        next_nonce += 1;
    }

    while let Some((tx_hash, transfer, result)) = futs.next().await {
        match result {
            Ok(response) => {
                let success = response.status().is_success();
                let text = response.text().await.ok();
                info!(target: "rebalancer", head_number = head.number, %head.hash, %signer_address, %tx_hash, rule = %transfer.description, success, response = ?text, "System transaction submitted");
            }
            Err(error) => {
                error!(target: "rebalancer", head_number = head.number, %head.hash, %signer_address, %tx_hash, rule = %transfer.description, ?error, "Error submitting system transaction")
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
struct Transfer {
    destination: Address,
    amount: U256,
    description: String,
}
