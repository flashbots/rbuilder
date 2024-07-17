use crate::{
    beacon_api_client::{Client, PayloadAttributesTopic},
    mev_boost::{ProposerPayloadDelivered, RelayClient, RelayError},
};
use alloy_network::EthereumWallet;
use alloy_primitives::{address, Address};
use alloy_signer_local::PrivateKeySigner;
use futures::StreamExt;
use primitive_types::H384;
use reth::rpc::types::beacon::events::PayloadAttributesEvent;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io,
    io::prelude::*,
    path::PathBuf,
    process::{Child, Command},
    str::FromStr,
    time::SystemTime,
};
use time::{format_description, format_description::well_known, OffsetDateTime};
use url::Url;

mod config;
use config::CONFIG;

#[derive(Debug)]
pub enum FakeMevBoostRelayError {
    SpawnError,
    BinaryNotFound,
}

pub struct FakeMevBoostRelay {}

impl Default for FakeMevBoostRelay {
    fn default() -> Self {
        Self::new()
    }
}

fn open_log_file(path: PathBuf) -> File {
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).unwrap();

    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap()
}

impl FakeMevBoostRelay {
    pub fn new() -> Self {
        Self {}
    }

    pub fn spawn(self) -> Option<FakeMevBoostRelayInstance> {
        self.try_spawn().unwrap()
    }

    fn try_spawn(self) -> Result<Option<FakeMevBoostRelayInstance>, FakeMevBoostRelayError> {
        let playground_dir = std::env::var("PLAYGROUND_DIR").unwrap();

        // append to the config template the paths to the playground
        let mut config = CONFIG.to_string();
        config.insert_str(
            0,
            format!("chain = \"{}/genesis.json\"\n", playground_dir).as_str(),
        );
        config.insert_str(
            0,
            format!("reth_datadir = \"{}/data_reth\"\n", playground_dir).as_str(),
        );

        // write the config into /tmp/rbuilder.toml
        let mut file = File::create("/tmp/rbuilder.toml").unwrap();
        file.write_all(config.as_bytes()).unwrap();

        // load the binary from the cargo_dir
        let mut bin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        bin_path.push("../../target/debug/rbuilder");

        let dt: OffsetDateTime = SystemTime::now().into();

        let format =
            format_description::parse("[year]_[month]_[day]_[hour]_[minute]_[second]").unwrap();
        let name = dt.format(&format).unwrap();

        let mut log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        log_path.push(format!("../../integration_logs/{}.log", name));

        let log = open_log_file(log_path);

        let mut cmd = Command::new(bin_path.clone());

        cmd.arg("run").arg("/tmp/rbuilder.toml");
        cmd.stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap());

        match cmd.spawn() {
            Ok(child) => Ok(Some(FakeMevBoostRelayInstance { child })),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Err(FakeMevBoostRelayError::BinaryNotFound),
                _ => Err(FakeMevBoostRelayError::SpawnError),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PayloadDelivered {
    slot: String,
    block_number: String,
    builder_pubkey: String,
}

#[derive(Debug)]
enum PayloadDeliveredError {
    Empty,
    IncorrectBuilder(H384),
    RelayError(RelayError),
}

#[derive(Debug)]
pub struct FakeMevBoostRelayInstance {
    child: Child,
}

impl Drop for FakeMevBoostRelayInstance {
    fn drop(&mut self) {
        self.child.kill().expect("could not kill mev-boost-server");
    }
}

impl FakeMevBoostRelayInstance {
    async fn wait_for_next_slot(
        &self,
    ) -> Result<PayloadAttributesEvent, Box<dyn std::error::Error>> {
        let client = Client::new(Url::parse("http://localhost:3500")?);
        let mut stream = client.get_events::<PayloadAttributesTopic>().await?;

        // wait for the next slot to send it so that it has enough time to build it.
        let event = stream.next().await.unwrap()?; // Fix unwrap
        Ok(event)
    }

    fn el_url(&self) -> &str {
        "http://localhost:8545"
    }

    fn prefunded_keys(&self, indx: usize) -> EthereumWallet {
        let signer: PrivateKeySigner =
            "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
                .parse()
                .unwrap();
        let wallet = EthereumWallet::from(signer);
        vec![wallet][indx].clone()
    }

    fn builder_address(&self) -> Address {
        address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
    }

    async fn validate_block_built(
        &self,
        block_number: u64,
    ) -> Result<ProposerPayloadDelivered, PayloadDeliveredError> {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // FIX,

        let client = RelayClient::from_url(
            Url::parse("http://localhost:5555").unwrap(),
            None,
            None,
            None,
        );

        let payload = client
            .proposer_payload_delivered_block_number(block_number)
            .await
            .map_err(|err| PayloadDeliveredError::RelayError(err))?
            .ok_or(PayloadDeliveredError::Empty)?;

        let builder_pubkey = H384::from_str("0xa1885d66bef164889a2e35845c3b626545d7b0e513efe335e97c3a45e534013fa3bc38c3b7e6143695aecc4872ac52c4").unwrap();
        if payload.builder_pubkey == builder_pubkey {
            Ok(payload)
        } else {
            Err(PayloadDeliveredError::IncorrectBuilder(
                payload.builder_pubkey,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_network::TransactionBuilder;
    use alloy_primitives::U256;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use url::Url;

    #[tokio::test]
    async fn test_simple_example() {
        let srv = FakeMevBoostRelay::new().spawn().unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // send a transfer to the builder
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(srv.prefunded_keys(0))
            .on_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await.unwrap();

        let tx = TransactionRequest::default()
            .with_to(srv.builder_address())
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);

        let pending_tx = provider.send_transaction(tx).await.unwrap();
        let receipt = pending_tx.get_receipt().await.unwrap();

        srv.validate_block_built(receipt.block_number.unwrap())
            .await
            .unwrap();
    }
}
