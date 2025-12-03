use crate::{
    beacon_api_client::{Client, PayloadAttributesTopic},
    mev_boost::{ProposerPayloadDelivered, RelayClient},
};
use alloy_network::EthereumWallet;
use alloy_primitives::{address, Address};
use alloy_rpc_types_beacon::{events::PayloadAttributesEvent, BlsPublicKey};
use alloy_signer_local::PrivateKeySigner;
use futures::StreamExt;
use std::{
    fs::{File, OpenOptions},
    io,
    io::prelude::*,
    path::PathBuf,
    process::{Child, Command},
    str::FromStr,
    thread,
    time::{Instant, SystemTime},
};
use time::{format_description, OffsetDateTime};
use url::Url;

#[derive(Debug)]
pub enum PlaygroundError {
    SpawnError,
    BinaryNotFound,
    SetupError,
    IntegrationPathNotFound,
    Timeout,
}

pub struct Playground {
    builder: Child,
}

fn open_log_file(path: PathBuf) -> io::Result<File> {
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).unwrap();

    OpenOptions::new().append(true).create(true).open(path)
}

impl Playground {
    pub fn new(name: &str, cfg_path: &PathBuf) -> Result<Self, PlaygroundError> {
        // load the binary from the cargo_dir
        let mut bin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        bin_path.push("../../target/debug/rbuilder");

        let dt: OffsetDateTime = SystemTime::now().into();

        let format = format_description::parse("[year]_[month]_[day]_[hour]_[minute]_[second]")
            .map_err(|_| PlaygroundError::SetupError)?;
        let format_str = dt
            .format(&format)
            .map_err(|_| PlaygroundError::SetupError)?;

        let mut log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        log_path.push(format!("../../integration_logs/{format_str}_{name}.log"));

        let log = open_log_file(log_path.clone()).map_err(|_| PlaygroundError::SetupError)?;
        let stdout = log.try_clone().map_err(|_| PlaygroundError::SetupError)?;
        let stderr = log.try_clone().map_err(|_| PlaygroundError::SetupError)?;

        let mut cmd = Command::new(bin_path.clone());

        cmd.arg("run").arg(cfg_path);
        cmd.stdout(stdout).stderr(stderr);

        let builder = match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Err(PlaygroundError::BinaryNotFound),
                _ => Err(PlaygroundError::SpawnError),
            },
        }?;

        let start = Instant::now();
        loop {
            if start.elapsed().as_secs() > 10 {
                return Err(PlaygroundError::Timeout);
            }

            // from the log file, check if the server has started
            // by checking if the string "RPC server job: started" is present
            let mut file = File::open(&log_path).map_err(|_| PlaygroundError::SetupError)?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|_| PlaygroundError::SetupError)?;

            if contents.contains("RPC server job: started") {
                break;
            }

            thread::sleep(std::time::Duration::from_millis(100));
        }

        Ok(Self { builder })
    }

    pub fn builder_is_alive(&mut self) -> bool {
        matches!(self.builder.try_wait(), Ok(None))
    }

    pub fn blocklist_key(&self) -> EthereumWallet {
        // Address: 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
        let signer: PrivateKeySigner =
            "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"
                .parse()
                .unwrap();
        EthereumWallet::from(signer)
    }

    pub fn blocklist_address(&self) -> Address {
        address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC")
    }

    pub async fn wait_for_next_slot(
        &self,
    ) -> Result<PayloadAttributesEvent, Box<dyn std::error::Error>> {
        let client = Client::new(Url::parse("http://localhost:3500")?);
        let mut stream = client.get_events::<PayloadAttributesTopic>().await?;

        // wait for the next slot to send it so that it has enough time to build it.
        let event = stream.next().await.unwrap()?; // Fix unwrap
        Ok(event)
    }

    pub fn rbuilder_rpc_url(&self) -> &str {
        "http://localhost:8645"
    }

    pub fn el_url(&self) -> &str {
        "http://localhost:8545"
    }

    pub fn prefunded_key(&self) -> EthereumWallet {
        // TODO: Return the full list of keys that we can use
        let signer: PrivateKeySigner =
            "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
                .parse()
                .unwrap();
        EthereumWallet::from(signer)
    }

    pub fn builder_address(&self) -> Address {
        address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
    }

    pub async fn validate_block_built(
        &self,
        block_number: u64,
    ) -> Result<ProposerPayloadDelivered, PayloadDeliveredError> {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // FIX,

        let client = RelayClient::from_url(
            Url::parse("http://localhost:5555").unwrap(),
            None,
            None,
            None,
            false,
            Vec::new(),
            false,
            false,
            false,
        );

        let payload = client
            .proposer_payload_delivered_block_number(block_number)
            .await
            .map_err(|_err| PayloadDeliveredError::RelayError)?
            .ok_or(PayloadDeliveredError::ProposalNotFound)?;

        let builder_pubkey = BlsPublicKey::from_str("0xa1885d66bef164889a2e35845c3b626545d7b0e513efe335e97c3a45e534013fa3bc38c3b7e6143695aecc4872ac52c4").unwrap();
        if payload.builder_pubkey == builder_pubkey {
            Ok(payload)
        } else {
            Err(PayloadDeliveredError::IncorrectBuilder)
        }
    }
}

#[derive(Debug)]
pub enum PayloadDeliveredError {
    ProposalNotFound,
    IncorrectBuilder,
    RelayError,
}

impl Drop for Playground {
    fn drop(&mut self) {
        self.builder
            .kill()
            .expect("could not kill mev-boost-server");
    }
}
