use alloy_eips::merge::SLOT_DURATION;
use alloy_primitives::B256;
use futures_util::Future;
use futures_util::FutureExt;
use reth::builder::BuilderContext;
use reth::providers::BlockReaderIdExt;
use reth::{providers::StateProviderFactory, tasks::TaskSpawner};
use reth_basic_payload_builder::BuildOutcome;
use reth_basic_payload_builder::HeaderForPayload;
use reth_basic_payload_builder::PayloadConfig;
use reth_basic_payload_builder::PayloadState;
use reth_basic_payload_builder::PayloadTaskGuard;
use reth_basic_payload_builder::PendingPayload;
use reth_basic_payload_builder::PrecachedState;
use reth_basic_payload_builder::ResolveBestPayload;
use reth_node_api::FullNodeTypes;
use reth_node_api::NodeTypesWithEngine;
use reth_node_api::PayloadBuilderAttributes;
use reth_node_api::PayloadKind;
use reth_node_api::PayloadTypes;
use reth_payload_builder::PayloadJobGenerator;
use reth_payload_builder::{KeepPayloadJobAlive, PayloadBuilderError, PayloadJob};
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::HeaderTy;
use reth_revm::cached::CachedReads;
use reth_revm::cancelled::CancelOnDrop;
use reth_revm::cancelled::ManualCancel;
use reth_transaction_pool::TransactionPool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::oneshot;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Interval;
use tokio::time::Sleep;
use tracing::debug;
use tracing::info;
use tracing::trace;

/// A type that knows how to build a payload builder to plug into [`BasicPayloadServiceBuilder`].
pub trait PayloadBuilderBuilder<Node: FullNodeTypes, Pool: TransactionPool>: Send + Sized {
    /// Payload builder implementation.
    type PayloadBuilder: PayloadBuilderFor<Node::Types> + Unpin + 'static;

    /// Spawns the payload service and returns the handle to it.
    ///
    /// The [`BuilderContext`] is provided to allow access to the node's configuration.
    fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> impl Future<Output = eyre::Result<Self::PayloadBuilder>> + Send;
}

/// Helper trait to bound [`PayloadBuilder`] to the node's engine types.
pub trait PayloadBuilderFor<N: NodeTypesWithEngine>:
    PayloadBuilder<
    Attributes = <N::Engine as PayloadTypes>::PayloadBuilderAttributes,
    BuiltPayload = <N::Engine as PayloadTypes>::BuiltPayload,
>
{
}

impl<T, N: NodeTypesWithEngine> PayloadBuilderFor<N> for T where
    T: PayloadBuilder<
        Attributes = <N::Engine as PayloadTypes>::PayloadBuilderAttributes,
        BuiltPayload = <N::Engine as PayloadTypes>::BuiltPayload,
    >
{
}

/// A trait for building payloads that encapsulate Ethereum transactions.
///
/// This trait provides the `try_build` method to construct a transaction payload
/// using `BuildArguments`. It returns a `Result` indicating success or a
/// `PayloadBuilderError` if building fails.
///
/// Generic parameters `Pool` and `Client` represent the transaction pool and
/// Ethereum client types.
pub trait PayloadBuilder: Send + Sync + Clone {
    /// The payload attributes type to accept for building.
    type Attributes: PayloadBuilderAttributes;
    /// The type of the built payload.
    type BuiltPayload: BuiltPayload;

    /// Tries to build a transaction payload using provided arguments.
    ///
    /// Constructs a transaction payload based on the given arguments,
    /// returning a `Result` indicating success or an error if building fails.
    ///
    /// # Arguments
    ///
    /// - `args`: Build arguments containing necessary components.
    ///
    /// # Returns
    ///
    /// A `Result` indicating the build outcome or an error.
    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError>;
}

/// Settings for the [`BasicPayloadJobGenerator`].
#[derive(Debug, Clone)]
pub struct BasicPayloadJobGeneratorConfig {
    /// The interval at which the job should build a new payload after the last.
    pub interval: Duration,
    /// The deadline for when the payload builder job should resolve.
    ///
    /// By default this is [`SLOT_DURATION`]: 12s
    pub deadline: Duration,
    /// Maximum number of tasks to spawn for building a payload.
    pub max_payload_tasks: usize,
}

// === impl BasicPayloadJobGeneratorConfig ===

impl BasicPayloadJobGeneratorConfig {
    /// Sets the interval at which the job should build a new payload after the last.
    pub const fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Sets the deadline when this job should resolve.
    pub const fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Sets the maximum number of tasks to spawn for building a payload(s).
    ///
    /// # Panics
    ///
    /// If `max_payload_tasks` is 0.
    pub fn max_payload_tasks(mut self, max_payload_tasks: usize) -> Self {
        assert!(
            max_payload_tasks > 0,
            "max_payload_tasks must be greater than 0"
        );
        self.max_payload_tasks = max_payload_tasks;
        self
    }
}

impl Default for BasicPayloadJobGeneratorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            // 12s slot time
            deadline: SLOT_DURATION,
            max_payload_tasks: 3,
        }
    }
}

/// The generator type that creates new jobs that builds empty blocks.
#[derive(Debug)]
pub struct BlockPayloadJobGenerator<Client, Tasks, Builder> {
    /// The client that can interact with the chain.
    client: Client,
    /// How to spawn building tasks
    executor: Tasks,
    /// The configuration for the job generator.
    config: BasicPayloadJobGeneratorConfig,
    /// Restricts how many generator tasks can be executed at once.
    payload_task_guard: PayloadTaskGuard,
    /// The type responsible for building payloads.
    ///
    /// See [PayloadBuilder]
    builder: Builder,
    /// Stored `cached_reads` for new payload jobs.
    pre_cached: Option<PrecachedState>,
}

// === impl EmptyBlockPayloadJobGenerator ===

impl<Client, Tasks, Builder> BlockPayloadJobGenerator<Client, Tasks, Builder> {
    /// Creates a new [`BasicPayloadJobGenerator`] with the given config and custom
    /// [`PayloadBuilder`]
    pub fn with_builder(
        client: Client,
        executor: Tasks,
        config: BasicPayloadJobGeneratorConfig,
        builder: Builder,
    ) -> Self {
        Self {
            client,
            executor,
            payload_task_guard: PayloadTaskGuard::new(config.max_payload_tasks),
            config,
            builder,
            pre_cached: None,
        }
    }

    /// Returns a reference to the tasks type
    pub const fn tasks(&self) -> &Tasks {
        &self.executor
    }

    /// Returns the pre-cached reads for the given parent header if it matches the cached state's
    /// block.
    fn maybe_pre_cached(&self, parent: B256) -> Option<CachedReads> {
        self.pre_cached
            .as_ref()
            .filter(|pc| pc.block == parent)
            .map(|pc| pc.cached.clone())
    }
}

impl<Client, Tasks, Builder> PayloadJobGenerator
    for BlockPayloadJobGenerator<Client, Tasks, Builder>
where
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = HeaderForPayload<Builder::BuiltPayload>>
        + Clone
        + Unpin
        + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type Job = BlockPayloadJob<Tasks, Builder>;

    /// This is invoked when the node receives payload attributes from the beacon node via
    /// `engine_forkchoiceUpdated`
    fn new_payload_job(
        &self,
        attributes: <Builder as PayloadBuilder>::Attributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let parent_header = if attributes.parent().is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .latest_header()
                .map_err(PayloadBuilderError::from)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
        } else {
            self.client
                .sealed_header_by_hash(attributes.parent())
                .map_err(PayloadBuilderError::from)?
                .ok_or_else(|| PayloadBuilderError::MissingParentHeader(attributes.parent()))?
        };

        info!("Spawn block building job");

        // The deadline is critical for payload availability. If we reach the deadline,
        // the payload job stops and cannot be queried again. With tight deadlines close
        // to the block number, we risk reaching the deadline before the node queries the payload.
        //
        // Adding 0.5 seconds as wiggle room since block times are shorter here.
        // TODO: A better long-term solution would be to implement cancellation logic
        // that cancels existing jobs when receiving new block building requests.
        let config = PayloadConfig::new(Arc::new(parent_header.clone()), attributes);

        let until = job_deadline(config.attributes.timestamp()) + Duration::from_millis(500);
        let deadline = Box::pin(tokio::time::sleep_until(until));

        let cached_reads = self.maybe_pre_cached(parent_header.hash());

        let mut job = BlockPayloadJob {
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            deadline,
            // ticks immediately
            _interval: tokio::time::interval(self.config.interval),
            best_payload: PayloadState::Missing,
            pending_block: None,
            cached_reads,
            payload_task_guard: self.payload_task_guard.clone(),
            metrics: Default::default(),
        };

        job.spawn_build_job();

        Ok(job)
    }
}

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use crate::metrics::PayloadBuilderMetrics;

#[derive(Debug)]
pub struct PendingBlock<T> {
    cancel: ManualCancel,
    pending: PendingPayload<T>,
}

/// A [PayloadJob] that builds empty blocks.
pub struct BlockPayloadJob<Tasks, Builder>
where
    Builder: PayloadBuilder,
{
    /// The configuration for how the payload will be created.
    config: PayloadConfig<Builder::Attributes, HeaderForPayload<Builder::BuiltPayload>>,
    /// How to spawn building tasks
    executor: Tasks,
    /// The deadline when this job should resolve.
    deadline: Pin<Box<Sleep>>,
    /// The interval at which the job should build a new payload after the last.
    _interval: Interval,
    /// The best payload so far and its state.
    best_payload: PayloadState<Builder::BuiltPayload>,
    /// Receiver for the block that is currently being built.
    pending_block: Option<PendingBlock<Builder::BuiltPayload>>,
    /// Restricts how many generator tasks can be executed at once.
    payload_task_guard: PayloadTaskGuard,
    /// Caches all disk reads for the state the new payloads builds on
    ///
    /// This is used to avoid reading the same state over and over again when new attempts are
    /// triggered, because during the building process we'll repeatedly execute the transactions.
    cached_reads: Option<CachedReads>,
    /// metrics for this type
    metrics: PayloadBuilderMetrics,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    builder: Builder,
}

impl<Tasks, Builder> PayloadJob for BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type PayloadAttributes = Builder::Attributes;
    type ResolvePayloadFuture = ResolveBestPayload<Self::BuiltPayload>;
    type BuiltPayload = Builder::BuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        if let Some(payload) = self.best_payload.payload() {
            Ok(payload.clone())
        } else {
            // No payload has been built yet, but we need to return something that the CL then
            // can deliver, so we need to return an empty payload.
            //
            // Note: it is assumed that this is unlikely to happen, as the payload job is
            // started right away and the first full block should have been
            // built by the time CL is requesting the payload.
            self.metrics.inc_requested_empty_payload();
            Err(PayloadBuilderError::MissingPayload)
        }
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        Ok(self.config.attributes.clone())
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        let best_payload = self.best_payload.payload().cloned();
        if best_payload.is_none() && self.pending_block.is_none() {
            // ensure we have a job scheduled if we don't have a best payload yet and none is active
            self.spawn_build_job();
        }

        let maybe_better = self.pending_block.take().map(|pending_block| {
            // cancel any pending blocks
            pending_block.cancel.cancel();
            pending_block.pending
        });
        let empty_payload = None;

        if best_payload.is_none() {
            info!(target: "payload_builder", id=%self.config.payload_id(), "no best payload yet to resolve");
        }

        let fut = ResolveBestPayload {
            best_payload,
            maybe_better,
            empty_payload: empty_payload.filter(|_| kind != PayloadKind::WaitForPending),
        };

        (fut, KeepPayloadJobAlive::No)
    }
}

pub struct BuildArguments<Attributes, Payload: BuiltPayload> {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes, HeaderTy<Payload::Primitives>>,
    /// A marker that can be used to cancel the job.
    pub cancel: ManualCancel,
    /// The best payload achieved so far.
    pub best_payload: Option<Payload>,
}

/// A [PayloadJob] is a future that's being polled by the `PayloadBuilderService`
impl<Tasks, Builder> BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    pub fn spawn_build_job(&mut self) {
        trace!(target: "payload_builder", id = %self.config.payload_id(), "spawn new payload build task");
        let (tx, rx) = oneshot::channel();
        let cancel = ManualCancel::default();
        let cancel_clone = cancel.clone();
        let guard = self.payload_task_guard.clone();
        let payload_config = self.config.clone();
        let best_payload = self.best_payload.payload().cloned();
        self.metrics.inc_initiated_payload_builds();
        let cached_reads = self.cached_reads.take().unwrap_or_default();
        let builder = self.builder.clone();
        self.executor.spawn_blocking(Box::pin(async move {
            // acquire the permit for executing the task
            let _permit = guard.acquire().await;
            let args = BuildArguments {
                cached_reads,
                config: payload_config,
                cancel,
                best_payload,
            };
            let result = builder.try_build(args);
            let _ = tx.send(result);
        }));
        self.pending_block = Some(PendingBlock {
            cancel: cancel_clone,
            pending: PendingPayload::new(CancelOnDrop::default(), rx),
        });
    }
}

/// A [PayloadJob] is a a future that's being polled by the `PayloadBuilderService`
impl<Tasks, Builder> Future for BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        tracing::trace!("Polling job");
        let this = self.get_mut();
        let (cancel, pending) = this
            .pending_block
            .take()
            .map(|pending_block| {
                (
                    Some(pending_block.cancel.clone()),
                    Some(pending_block.pending),
                )
            })
            .unwrap_or_default();
        // check if the deadline is reached
        if this.deadline.as_mut().poll(cx).is_ready() {
            trace!(target: "payload_builder", "payload building deadline reached");
            if let Some(cancel) = cancel {
                cancel.cancel();
            }
            return Poll::Ready(Ok(()));
        }

        // poll the pending block
        if let Some(mut fut) = pending {
            match fut.poll_unpin(cx) {
                Poll::Ready(Ok(outcome)) => match outcome {
                    BuildOutcome::Better {
                        payload,
                        cached_reads,
                    } => {
                        this.cached_reads = Some(cached_reads);
                        debug!(target: "payload_builder", value = %payload.fees(), "built better payload");
                        this.best_payload = PayloadState::Best(payload);
                    }
                    BuildOutcome::Freeze(payload) => {
                        debug!(target: "payload_builder", "payload frozen, no further building will occur");
                        this.best_payload = PayloadState::Frozen(payload);
                    }
                    BuildOutcome::Aborted { fees, cached_reads } => {
                        this.cached_reads = Some(cached_reads);
                        trace!(target: "payload_builder", worse_fees = %fees, "skipped payload build of worse block");
                    }
                    BuildOutcome::Cancelled => {
                        unreachable!("the cancel signal never fired")
                    }
                },
                Poll::Ready(Err(error)) => {
                    // job failed, but we simply try again next interval
                    debug!(target: "payload_builder", %error, "payload build attempt failed");
                    this.metrics.inc_failed_payload_builds();
                }
                Poll::Pending => {
                    this.pending_block = Some(PendingBlock {
                        cancel: cancel.unwrap(),
                        pending: fut,
                    });
                }
            }
        }

        Poll::Pending
    }
}

// A future that resolves when a payload becomes available in the BlockCell
pub struct ResolvePayload<T> {
    future: WaitForValue<T>,
}

impl<T> ResolvePayload<T> {
    pub fn new(future: WaitForValue<T>) -> Self {
        Self { future }
    }
}

impl<T: Clone> Future for ResolvePayload<T> {
    type Output = Result<T, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut().future.poll_unpin(cx) {
            Poll::Ready(value) => Poll::Ready(Ok(value)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct BlockCell<T> {
    inner: Arc<Mutex<Option<T>>>,
    notify: Arc<Notify>,
}

impl<T: Clone> BlockCell<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn is_some(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.is_some()
    }

    pub fn set(&self, value: T) {
        let mut inner = self.inner.lock().unwrap();
        *inner = Some(value);
        self.notify.notify_one();
    }

    pub fn get(&self) -> Option<T> {
        let inner = self.inner.lock().unwrap();
        inner.clone()
    }

    // Return a future that resolves when value is set
    pub fn wait_for_value(&self) -> WaitForValue<T> {
        WaitForValue { cell: self.clone() }
    }
}

#[derive(Clone)]
// Future that resolves when a value is set in BlockCell
pub struct WaitForValue<T> {
    cell: BlockCell<T>,
}

impl<T: Clone> Future for WaitForValue<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = self.cell.get() {
            Poll::Ready(value)
        } else {
            // Instead of register, we use notified() to get a future
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl<T: Clone> Default for BlockCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn job_deadline(unix_timestamp_secs: u64) -> Instant {
    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Safe subtraction that handles the case where timestamp is in the past
    let duration_until = unix_timestamp_secs.saturating_sub(unix_now);

    if duration_until == 0 {
        // Enforce a minimum block time of 1 second by rounding up any duration less than 1 second
        Instant::now() + Duration::from_secs(1)
    } else {
        Instant::now() + Duration::from_secs(duration_until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use rand::thread_rng;
    use reth::tasks::TokioTaskExecutor;
    use reth_chain_state::ExecutedBlockWithTrieUpdates;
    use reth_node_api::NodePrimitives;
    use reth_optimism_node::{OpBuiltPayload, OpEngineTypes};
    use reth_optimism_payload_builder::payload::OpPayloadBuilderAttributes;
    use reth_optimism_payload_builder::OpPayloadPrimitives;
    use reth_optimism_primitives::OpPrimitives;
    use reth_payload_builder::PayloadBuilderService;
    use reth_primitives::SealedBlock;
    use reth_provider::test_utils::MockEthProvider;
    use reth_provider::CanonStateSubscriptions;
    use reth_testing_utils::generators::{random_block_range, BlockRangeParams};
    use tokio::task;
    use tokio::time::{sleep, Duration};
    use tracing::Level;

    #[tokio::test]
    async fn test_block_cell_wait_for_value() {
        let cell = BlockCell::new();

        // Spawn a task that will set the value after a delay
        let cell_clone = cell.clone();
        task::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            cell_clone.set(42);
        });

        // Wait for the value and verify
        let wait_future = cell.wait_for_value();
        let result = wait_future.await;
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_block_cell_immediate_value() {
        let cell = BlockCell::new();
        cell.set(42);

        // Value should be immediately available
        let wait_future = cell.wait_for_value();
        let result = wait_future.await;
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_block_cell_multiple_waiters() {
        let cell = BlockCell::new();

        // Spawn multiple waiters
        let wait1 = task::spawn({
            let cell = cell.clone();
            async move { cell.wait_for_value().await }
        });

        let wait2 = task::spawn({
            let cell = cell.clone();
            async move { cell.wait_for_value().await }
        });

        // Set value after a delay
        sleep(Duration::from_millis(100)).await;
        cell.set(42);

        // All waiters should receive the value
        assert_eq!(wait1.await.unwrap(), 42);
        assert_eq!(wait2.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_block_cell_update_value() {
        let cell = BlockCell::new();

        // Set initial value
        cell.set(42);

        // Set new value
        cell.set(43);

        // Waiter should get the latest value
        let result = cell.wait_for_value().await;
        assert_eq!(result, 43);
    }

    #[derive(Debug, Clone)]
    struct MockBuilder<N> {
        events: Arc<Mutex<Vec<BlockEvent>>>,
        _marker: std::marker::PhantomData<N>,
    }

    impl<N> MockBuilder<N> {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(vec![])),
                _marker: std::marker::PhantomData,
            }
        }

        fn new_event(&self, event: BlockEvent) {
            let mut events = self.events.lock().unwrap();
            events.push(event);
        }

        fn get_events(&self) -> Vec<BlockEvent> {
            let mut events = self.events.lock().unwrap();
            std::mem::take(&mut *events)
        }
    }

    #[derive(Debug, PartialEq, Clone)]
    enum BlockEvent {
        Started,
        Cancelled,
    }

    impl<N> PayloadBuilder for MockBuilder<N>
    where
        N: OpPayloadPrimitives,
    {
        type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
        type BuiltPayload = OpBuiltPayload<N>;

        fn try_build(
            &self,
            args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
            self.new_event(BlockEvent::Started);

            loop {
                if args.cancel.is_cancelled() {
                    self.new_event(BlockEvent::Cancelled);
                    return Ok(BuildOutcome::Cancelled);
                }

                // Small sleep to prevent tight loop
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[tokio::test]
    async fn test_job_deadline() {
        // Test future deadline
        let now = Instant::now();
        // Get current Unix time
        let current_unix_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 2 seconds in the future
        let future_unix_timestamp = current_unix_time + 2;
        let deadline = job_deadline(future_unix_timestamp);
        assert!(deadline.duration_since(now).as_secs() <= 2);
        assert!(deadline > now);

        // Test past deadline
        let past_unix_timestamp = current_unix_time - 10;
        let deadline = job_deadline(past_unix_timestamp);
        // Should default to 1 second when timestamp is in the past
        assert_eq!(deadline.duration_since(now).as_secs(), 1);

        // Test current timestamp
        let deadline = job_deadline(current_unix_time);
        // Should use 1 second when timestamp is current
        assert_eq!(deadline.duration_since(now).as_secs(), 1);
    }

    #[tokio::test]
    async fn test_payload_generator() -> eyre::Result<()> {
        tracing::subscriber::set_global_default(tracing_subscriber::fmt().finish()).unwrap();

        let mut rng = thread_rng();

        let client = MockEthProvider::default();
        let executor = TokioTaskExecutor::default();
        let config = BasicPayloadJobGeneratorConfig::default().max_payload_tasks(1);
        let builder = MockBuilder::<OpPrimitives>::new();

        let (start, count) = (1, 10);
        let blocks = random_block_range(
            &mut rng,
            start..=start + count - 1,
            BlockRangeParams {
                tx_count: 0..2,
                ..Default::default()
            },
        );

        client.extend_blocks(blocks.iter().cloned().map(|b| (b.hash(), b.unseal())));

        let generator = BlockPayloadJobGenerator::with_builder(
            client.clone(),
            executor,
            config,
            builder.clone(),
        );

        let (payload_service, payload_service_handle) =
            PayloadBuilderService::<_, _, OpEngineTypes>::new(
                generator,
                client.canonical_state_stream(),
            );
        tokio::spawn(async move {
            let _ = payload_service.await;
        });

        // this is not nice but necessary
        let mut attr = OpPayloadBuilderAttributes::default();
        attr.payload_attributes.parent = client.latest_header()?.unwrap().hash();

        let _ = payload_service_handle.send_new_payload(attr.clone()).await;
        // job resolve triggers cancellations from the build task
        let _ = payload_service_handle
            .resolve_kind(attr.payload_id(), PayloadKind::Earliest)
            .await;
        let events = builder.get_events();
        assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);

        let _ = payload_service_handle.send_new_payload(attr.clone()).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let events: Vec<BlockEvent> = builder.get_events();
        assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);

        Ok(())
    }
}
