use crate::{
    building::priority_update::priority_update_pool::PriorityUpdateIngressOrderpool,
    provider::StateProviderFactory,
};
use alloy_consensus::Transaction as TransactionTrait;
use alloy_primitives::{B256, U256};
use rbuilder_primitives::{
    proto::builder_priority_update_v1::{
        builder_priority_update_service_server::{
            BuilderPriorityUpdateService, BuilderPriorityUpdateServiceServer,
        },
        PriorityUpdate, PriorityUpdatePlacement, PriorityUpdateResponse,
    },
    serialize::TxEncoding,
    Bundle, BundleReplacementData, BundleReplacementKey, Metadata, Order, PriorityUpdateData,
    PriorityUpdateKind, TransactionSignedEcRecoveredWithBlobs, TxWithBlobsCreateError,
    LAST_BUNDLE_VERSION,
};
use reth_errors::ProviderError;
use reth_primitives::Account;
use reth_provider::AccountReader;
use std::{mem, net::SocketAddr, sync::Arc};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{error, info, trace, warn};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AddPriorityUpdateError {
    #[error("invalid replacement_uuid")]
    InvalidUuid(uuid::Error),
    #[error("block number out of range")]
    BlockNumberOutOfRange { target: u64, head: u64 },
    #[error("invalid tx")]
    InvalidTx(TxWithBlobsCreateError),
    #[error("invalid nonce")]
    InvalidNonce,
    #[error("insufficient balance")]
    InsufficientBalance,
    #[error("invalid external_hash")]
    InvalidExternalHash,
    #[error("internal error")]
    Internal,
}

#[derive(Debug)]
pub struct PriorityUpdateGrpcService<P> {
    pool: PriorityUpdateIngressOrderpool,
    provider: P,
}

impl<P> PriorityUpdateGrpcService<P> {
    pub fn new(pool: PriorityUpdateIngressOrderpool, provider: P) -> Self {
        Self { pool, provider }
    }
}

impl<P> PriorityUpdateGrpcService<P>
where
    P: StateProviderFactory,
{
    fn process(&self, mut update: PriorityUpdate) -> Result<(), AddPriorityUpdateError> {
        let uuid = Uuid::from_slice(&update.replacement_uuid)
            .map_err(AddPriorityUpdateError::InvalidUuid)?;

        let last_block = self
            .provider
            .last_block_number()
            .map_err(map_provider_error)?;
        if update.block_number != last_block + 1 && update.block_number != last_block + 2 {
            return Err(AddPriorityUpdateError::BlockNumberOutOfRange {
                target: update.block_number,
                head: last_block,
            });
        }

        let block_number = update.block_number;
        let seq = update.replacement_seq_number;

        let value = if update.tx.is_empty() {
            None
        } else {
            let external_hash = parse_external_hash(&update.external_hash)?;
            let tx_bytes = mem::take(&mut update.tx);
            let tx = TxEncoding::WithBlobData
                .decode(tx_bytes.into())
                .map_err(AddPriorityUpdateError::InvalidTx)?;
            let signer = tx.signer();
            let state = self.provider.latest().map_err(map_provider_error)?;
            let account = state
                .basic_account(&signer)
                .map_err(map_provider_error)?
                .unwrap_or_default();
            validate_tx_account_state(&tx, &account)?;
            Some(build_priority_update_order(update, uuid, tx, external_hash))
        };

        self.pool.add_event(block_number, uuid, seq, value);
        Ok(())
    }
}

#[tonic::async_trait]
impl<P> BuilderPriorityUpdateService for PriorityUpdateGrpcService<P>
where
    P: StateProviderFactory + 'static,
{
    async fn submit_priority_update(
        &self,
        request: Request<PriorityUpdate>,
    ) -> Result<Response<PriorityUpdateResponse>, Status> {
        let req = request.into_inner();
        let block_number = req.block_number;
        let seq = req.replacement_seq_number;
        let uuid = Uuid::from_slice(&req.replacement_uuid).ok();
        let result = self.process(req);
        trace!(
            ?uuid,
            seq,
            block_number,
            err = ?result.as_ref().err(),
            "priority update processed"
        );
        let response = match result {
            Ok(()) => PriorityUpdateResponse {
                error: String::new(),
            },
            Err(err) => PriorityUpdateResponse {
                error: err.to_string(),
            },
        };
        Ok(Response::new(response))
    }
}

fn validate_tx_account_state(
    tx: &TransactionSignedEcRecoveredWithBlobs,
    account: &Account,
) -> Result<(), AddPriorityUpdateError> {
    if tx.nonce() != account.nonce {
        return Err(AddPriorityUpdateError::InvalidNonce);
    }

    let inner = tx.internal_tx_unsecure();
    let max_gas_cost =
        U256::from(inner.gas_limit()).saturating_mul(U256::from(inner.max_fee_per_gas()));
    let max_blob_cost = match inner.max_fee_per_blob_gas() {
        Some(blob_fee) => U256::from(tx.blobs_gas_used()).saturating_mul(U256::from(blob_fee)),
        None => U256::ZERO,
    };
    let total_cost = tx
        .value()
        .saturating_add(max_gas_cost)
        .saturating_add(max_blob_cost);

    if account.balance < total_cost {
        return Err(AddPriorityUpdateError::InsufficientBalance);
    }

    Ok(())
}

fn parse_external_hash(bytes: &[u8]) -> Result<B256, AddPriorityUpdateError> {
    B256::try_from(bytes).map_err(|_| AddPriorityUpdateError::InvalidExternalHash)
}

fn build_priority_update_order(
    update: PriorityUpdate,
    uuid: Uuid,
    tx: TransactionSignedEcRecoveredWithBlobs,
    external_hash: B256,
) -> Arc<Order> {
    let kind = match PriorityUpdatePlacement::try_from(update.placement) {
        Ok(PriorityUpdatePlacement::AlwaysTopOfBlock) => PriorityUpdateKind::ForceTopOfBlock,
        _ => PriorityUpdateKind::Regular,
    };

    let mut bundle = Bundle {
        version: LAST_BUNDLE_VERSION,
        block: Some(update.block_number),
        min_timestamp: None,
        max_timestamp: None,
        txs: vec![tx],
        reverting_tx_hashes: Vec::new(),
        dropping_tx_hashes: Vec::new(),
        hash: Default::default(),
        uuid: Default::default(),
        replacement_data: Some(BundleReplacementData {
            key: BundleReplacementKey::new(uuid, None),
            sequence_number: update.replacement_seq_number,
        }),
        signer: None,
        refund_identity: None,
        metadata: Metadata {
            priority_update_data: Some(PriorityUpdateData {
                kind,
                source: update.source,
            }),
            ..Metadata::new_received_now()
        },
        refund: None,
        external_hash: Some(external_hash),
    };
    bundle.hash_slow();
    Arc::new(Order::Bundle(bundle))
}

pub fn start_priority_update_grpc_server<P>(
    addr: SocketAddr,
    pool: PriorityUpdateIngressOrderpool,
    provider: P,
    global_cancel: CancellationToken,
) -> JoinHandle<()>
where
    P: StateProviderFactory + 'static,
{
    tokio::spawn(async move {
        info!(%addr, "PriorityUpdateGrpcServer: starting");
        let result = Server::builder()
            .add_service(BuilderPriorityUpdateServiceServer::new(
                PriorityUpdateGrpcService::new(pool, provider),
            ))
            .serve_with_shutdown(addr, async {
                global_cancel.cancelled().await;
            })
            .await;
        if let Err(err) = result {
            error!(?err, "PriorityUpdateGrpcServer: terminated with error");
        } else {
            info!("PriorityUpdateGrpcServer: shut down");
        }
    })
}

fn map_provider_error(err: ProviderError) -> AddPriorityUpdateError {
    warn!(?err, "provider error");
    AddPriorityUpdateError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::Signer;
    use alloy_consensus::TxLegacy;
    use reth_primitives::Transaction;

    const TEST_GAS_LIMIT: u64 = 21_000;
    const TEST_GAS_PRICE: u128 = 1_000_000_000;

    fn signed_tx(nonce: u64) -> TransactionSignedEcRecoveredWithBlobs {
        let signer = Signer::random();
        let recovered = signer
            .sign_tx(Transaction::Legacy(TxLegacy {
                nonce,
                gas_limit: TEST_GAS_LIMIT,
                gas_price: TEST_GAS_PRICE,
                value: U256::ZERO,
                ..Default::default()
            }))
            .unwrap();
        TransactionSignedEcRecoveredWithBlobs::new_no_blobs(recovered).unwrap()
    }

    #[test]
    fn validates_matching_nonce_and_sufficient_balance() {
        let tx = signed_tx(0);
        let account = Account {
            nonce: 0,
            balance: U256::MAX,
            bytecode_hash: None,
        };
        validate_tx_account_state(&tx, &account).unwrap();
    }

    #[test]
    fn rejects_wrong_nonce() {
        let tx = signed_tx(0);
        let account = Account {
            nonce: 7,
            balance: U256::MAX,
            bytecode_hash: None,
        };
        assert!(matches!(
            validate_tx_account_state(&tx, &account),
            Err(AddPriorityUpdateError::InvalidNonce)
        ));
    }

    #[test]
    fn rejects_insufficient_balance() {
        let tx = signed_tx(0);
        let account = Account {
            nonce: 0,
            balance: U256::ZERO,
            bytecode_hash: None,
        };
        assert!(matches!(
            validate_tx_account_state(&tx, &account),
            Err(AddPriorityUpdateError::InsufficientBalance)
        ));
    }

    #[test]
    fn external_hash_32_bytes_decodes() {
        let bytes = [7u8; 32];
        assert_eq!(parse_external_hash(&bytes).unwrap(), B256::from(bytes));
    }

    #[test]
    fn external_hash_wrong_length_is_rejected() {
        for bad in [&[][..], &[1, 2, 3], &[0u8; 31], &[0u8; 33]] {
            assert!(matches!(
                parse_external_hash(bad),
                Err(AddPriorityUpdateError::InvalidExternalHash)
            ));
        }
    }
}
