use std::collections::VecDeque;

use iceoryx2::{
    prelude::ZeroCopySend,
    service::{ipc, port_factory::publish_subscribe},
};
use rbuilder::utils::offset_datetime_to_timestamp_us;
use time::OffsetDateTime;
use tracing::info;

use crate::bidding_service_wrapper::fast_streams::{helpers::Error, types::WithCreationTime};

/// Super simple struct to collect duration stats (although it can be used for any u64 value).
/// It keeps the last window_size samples to answer the avg value.
struct DurationStats {
    sum: u64,
    durations: VecDeque<u64>,
    window_size: usize,
}

impl DurationStats {
    fn new(window_size: usize) -> Self {
        Self {
            sum: 0,
            durations: VecDeque::new(),
            window_size,
        }
    }

    fn add_duration(&mut self, duration: u64) {
        self.sum += duration;
        self.durations.push_back(duration);
        if self.durations.len() > self.window_size {
            self.sum -= self.durations.pop_front().unwrap();
        }
    }

    fn average_duration(&self) -> f64 {
        self.sum as f64 / self.durations.len() as f64
    }
}

const DURATION_STATS_WINDOW_SIZE: usize = 100;
/// Helper to poll a subscriber and collect some metrics if needed.
pub struct SubscriberPoller<T: std::fmt::Debug + ZeroCopySend + 'static> {
    subscriber: iceoryx2::port::subscriber::Subscriber<ipc::Service, T, ()>,
    flight_stats: DurationStats,
    poll_duration_stats: DurationStats,
    total_samples: u64,
    name: &'static str,
}

impl<T: std::fmt::Debug + ZeroCopySend + Copy> SubscriberPoller<T> {
    pub fn new(
        service: publish_subscribe::PortFactory<ipc::Service, T, ()>,
        buffer_size: usize,
        name: &'static str,
    ) -> Result<Self, Error> {
        let subscriber = service
            .subscriber_builder()
            .buffer_size(buffer_size)
            .create()?;
        Ok(Self {
            subscriber,
            flight_stats: DurationStats::new(DURATION_STATS_WINDOW_SIZE),
            poll_duration_stats: DurationStats::new(DURATION_STATS_WINDOW_SIZE),
            total_samples: 0,
            name,
        })
    }

    /// Poll the subscriber and calls process_sample on each sample.
    /// Stops polling on any error.
    pub fn poll(&mut self, process_sample: impl Fn(T)) -> Result<(), Error> {
        while let Some(sample) = self.subscriber.receive()? {
            process_sample(*sample);
        }
        Ok(())
    }
}

impl<T: std::fmt::Debug + WithCreationTime + ZeroCopySend + Copy> SubscriberPoller<T> {
    const METRICS_TRACE_INTERVAL: u64 = 100;
    /// Same as ['poll'] but also collects metrics that traces every METRICS_TRACE_INTERVAL samples.
    pub fn poll_with_metrics(&mut self, process_sample: impl Fn(T)) -> Result<(), Error> {
        let start = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc());
        self.total_samples += 1;
        while let Some(sample) = self.subscriber.receive()? {
            let now = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc());
            let delta = now - sample.creation_time_us();
            self.flight_stats.add_duration(delta);
            process_sample(*sample);
        }
        let delta = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc()) - start;
        self.poll_duration_stats.add_duration(delta);
        if self.total_samples % Self::METRICS_TRACE_INTERVAL == 0 {
            info!(
                name = self.name,
                avg_flight_time_us = self.flight_stats.average_duration(),
                avg_poll_time_us = self.poll_duration_stats.average_duration(),
                "Polling stats",
            );
        }
        Ok(())
    }
}
