use prometheus::{
    core::{Collector, Desc},
    proto::MetricFamily,
    IntCounter, IntCounterVec, IntGauge, Opts, Registry,
};
use tokio::runtime::Handle;

pub struct TokioMetricsCollector {
    // Gauge metrics
    num_workers: IntGauge,
    num_alive_tasks: IntGauge,
    global_queue_depth: IntGauge,

    // Counter metrics
    worker_park_count: IntCounterVec,
    worker_park_unpark_count: IntCounterVec,
    worker_total_busy_duration_nanos: IntCounterVec,
}

impl TokioMetricsCollector {
    pub fn new() -> prometheus::Result<Self> {
        Ok(Self {
            num_workers: IntGauge::with_opts(Opts::new(
                "tokio_runtime_workers_count",
                "Number of worker threads",
            ))?,
            num_alive_tasks: IntGauge::with_opts(Opts::new(
                "tokio_runtime_num_alive_tasks",
                "Number of alive tasks",
            ))?,
            global_queue_depth: IntGauge::with_opts(Opts::new(
                "tokio_runtime_global_queue_depth",
                "Tasks in global queue",
            ))?,
            worker_park_count: IntCounterVec::new(
                Opts::new(
                    "tokio_runtime_worker_park_count_total",
                    "Total times worker parked",
                ),
                &["worker"],
            )?,
            worker_park_unpark_count: IntCounterVec::new(
                Opts::new(
                    "tokio_runtime_worker_park_unpark_count_total",
                    "Total times worker parked and unparked",
                ),
                &["worker"],
            )?,
            worker_total_busy_duration_nanos: IntCounterVec::new(
                Opts::new(
                    "tokio_runtime_worker_busy_duration_nanos_total",
                    "Total busy duration in nanoseconds",
                ),
                &["worker"],
            )?,
        })
    }

    fn update_counter(counter: &IntCounter, tokio_value: u64) {
        let counter_value = counter.get();
        let delta = tokio_value.saturating_sub(counter_value);
        if delta > 0 {
            counter.inc_by(delta);
        }
    }
}

impl Collector for TokioMetricsCollector {
    fn desc(&self) -> Vec<&Desc> {
        let mut descs = Vec::new();
        descs.extend(self.num_workers.desc());
        descs.extend(self.num_alive_tasks.desc());
        descs.extend(self.global_queue_depth.desc());
        descs.extend(self.worker_park_count.desc());
        descs.extend(self.worker_park_unpark_count.desc());
        descs.extend(self.worker_total_busy_duration_nanos.desc());
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        if let Ok(handle) = Handle::try_current() {
            let metrics = handle.metrics();

            // Update gauges
            self.num_workers.set(metrics.num_workers() as i64);
            self.num_alive_tasks.set(metrics.num_alive_tasks() as i64);
            self.global_queue_depth
                .set(metrics.global_queue_depth() as i64);

            // Update counters with deltas
            for worker in 0..metrics.num_workers() {
                let label = worker.to_string();

                Self::update_counter(
                    &self.worker_park_count.with_label_values(&[&label]),
                    metrics.worker_park_count(worker),
                );

                Self::update_counter(
                    &self.worker_park_unpark_count.with_label_values(&[&label]),
                    metrics.worker_park_unpark_count(worker),
                );

                Self::update_counter(
                    &self
                        .worker_total_busy_duration_nanos
                        .with_label_values(&[&label]),
                    metrics.worker_total_busy_duration(worker).as_nanos() as u64,
                );
            }
        }

        let mut families = Vec::new();
        families.extend(self.num_workers.collect());
        families.extend(self.num_alive_tasks.collect());
        families.extend(self.global_queue_depth.collect());
        families.extend(self.worker_park_count.collect());
        families.extend(self.worker_park_unpark_count.collect());
        families.extend(self.worker_total_busy_duration_nanos.collect());

        families
    }
}

pub fn register_tokio_metrics(registry: &Registry) -> prometheus::Result<()> {
    registry.register(Box::new(TokioMetricsCollector::new()?))
}
