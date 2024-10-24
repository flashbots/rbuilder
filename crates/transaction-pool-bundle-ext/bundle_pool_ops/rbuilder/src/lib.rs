//! Implementation of [`BundlePoolOperations`] for the classic rbuilder that
//! supports [`EthSendBundle`]s.

// #![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{sync::Arc, time::Duration};

use derive_more::From;
use rbuilder::{
    building::{builders::{ordering_builder::OrderingBuilderConfig, UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory}, Sorting},
    live_builder::{base_config::{load_config_toml_and_env, BaseConfig}, cli::LiveBuilderConfig, config::{create_builders, BuilderConfig, Config, SpecificBuilderConfig}, payload_events::MevBoostSlotData, SlotSource},
};
use reth_db_api::Database;
use reth_primitives::{Bytes, B256};
use reth_provider::{DatabaseProviderFactory, HeaderProvider, StateProviderFactory};
use reth_rpc_types::{beacon::events::PayloadAttributesEvent, mev::EthSendBundle};
use tokio::{sync::mpsc::{self, error::SendError}, task, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::error;
use transaction_pool_bundle_ext::BundlePoolOperations;

/// [`BundlePoolOperations`] implementation which uses components of the
/// [`rbuilder`] under the hood to handle classic [`EthSendBundle`]s.
#[allow(unused)]
#[derive(Debug)]
pub struct BundlePoolOps {
    payload_attributes_tx: mpsc::UnboundedSender<(PayloadAttributesEvent, Option<u64>)>,
}

#[derive(Debug)]
struct OurSlotSource {
    payload_attributes_rx: mpsc::UnboundedReceiver<(PayloadAttributesEvent, Option<u64>)>,
}

impl SlotSource for OurSlotSource {
    fn recv_slot_channel(self) -> mpsc::UnboundedReceiver<MevBoostSlotData> {
        let (slot_sender, slot_receiver) = mpsc::unbounded_channel();

        // Spawn a task that receives payload attributes, converts them
        // into [`MevBoostSlotData`] for rbuilder, then forwards them.
        tokio::spawn(async move {
            let mut recv = self.payload_attributes_rx;
            while let Some((payload_event, gas_limit)) = recv.recv().await {
                let mev_boost_data = MevBoostSlotData {
                    payload_attributes_event: payload_event,
                    suggested_gas_limit: gas_limit.unwrap_or(0),
                    relays: vec![],
                    slot_data: Default::default(),
                };

                if slot_sender.send(mev_boost_data).is_err() {
                    error!("Error sending MevBoostSlotData through channel");
                    break;
                }
            }
        });

        // Return the receiver end for SlotSource trait
        slot_receiver
    }
}

impl BundlePoolOps {
    pub async fn new<P, DB>(provider: P) -> Result<Self, Error> where
        DB: Database + Clone + 'static,
        P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
    {
        // Create the payload source to trigger new block building
        let cancellation_token = CancellationToken::new();
        let (payload_attributes_tx, payload_attributes_rx) = mpsc::unbounded_channel();
        let slot_source = OurSlotSource {
            payload_attributes_rx,
        };

        let sink_factory = SinkFactory {};

        // Spawn the builder!
        let config: Config = load_config_toml_and_env("/Users/liamaharon/grimoire/rbuilder/config-optimism-local.toml")?;

        // Spawn the task in a separate thread of execution, allowing it to run without blocking.
        let _handle = task::spawn(async move {
            // Wait for 5 seconds without blocking
            sleep(Duration::from_secs(5)).await;

            let builder_strategy =
                BuilderConfig {
                    name: "mp-ordering".to_string(),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        coinbase_payment: false,
                        build_duration_deadline_ms: None,
                    }),
                };

                    let builders = create_builders(
            vec![builder_strategy],
            config.base_config.live_root_hash_config().unwrap(),
            config.base_config.root_hash_task_pool().unwrap(),
            config.base_config.sbundle_mergeabe_signers(),
        );


            // Build and run the process
            let builder = config.base_config
                .create_builder_with_provider_factory::<P, DB, OurSlotSource>(
                    cancellation_token,
                    Box::new(sink_factory),
                    slot_source,
                    provider,
                )
                .await.unwrap().with_builders(builders);

            builder.run().await.unwrap();

            Ok::<(), ()>
        });

        Ok(BundlePoolOps {
            payload_attributes_tx,
        })
    }
}

impl BundlePoolOperations for BundlePoolOps {
    /// Signed eth transaction
    type Transaction = Bytes;
    type Bundle = EthSendBundle;
    type Error = Error;

    fn add_bundle(&self, _bundle: Self::Bundle) -> Result<(), Self::Error> {
        // TODO: Add bundle to live_builder OrderPool
        todo!()
    }

    fn cancel_bundle(&self, _hash: &B256) -> Result<(), Self::Error> {
        // TODO: Cancel bundle from live_builder OrderPool
        todo!()
    }

    fn get_transactions(&self) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error> {
        // TODO: Return transactions from live_builder BlockBuilderSink
        Ok(vec![])
    }

    fn notify_payload_attributes_event(
        &self,
        payload_attributes: PayloadAttributesEvent,
        gas_limit: Option<u64>,
    ) -> Result<(), Self::Error> {
        self.payload_attributes_tx
            .send((payload_attributes, gas_limit))?;
        Ok(())
    }
}

#[derive(Debug)]
struct Sink {}

#[derive(Debug)]
struct SinkFactory {}

impl UnfinishedBlockBuildingSinkFactory for SinkFactory {
    fn create_sink(
        &mut self,
        _slot_data: MevBoostSlotData,
        _cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink> {
        Arc::new(Sink {})
    }
}

impl UnfinishedBlockBuildingSink for Sink {
    fn new_block(
        &self,
        block: Box<dyn rbuilder::building::builders::block_building_helper::BlockBuildingHelper>,
    ) {
        dbg!("Made a block!!", block.built_block_trace());
        // todo!()
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        true
    }
}

/// [`BundlePoolOperations`] error type.
#[derive(Debug, From)]
pub enum Error {
    #[from]
    Eyre(eyre::Error),

    #[from]
    SendPayloadAttributes(SendError<(PayloadAttributesEvent, Option<u64>)>),
}
