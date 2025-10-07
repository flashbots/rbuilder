use std::collections::VecDeque;

use iceoryx2::{
    prelude::ZeroCopySend,
    service::{ipc, port_factory::publish_subscribe},
};
use rbuilder::utils::offset_datetime_to_timestamp_us;
use time::OffsetDateTime;
use tracing::info;

use crate::bidding_service_wrapper::fast_streams::types::WithCreationTime;

struct DurationStats {
    sum: u64,
    durations: VecDeque<u64>,
}

impl DurationStats {
    fn new() -> Self {
        Self {
            sum: 0,
            durations: VecDeque::new(),
        }
    }

    fn add_duration(&mut self, duration: u64) {
        self.sum += duration;
        self.durations.push_back(duration);
        if self.durations.len() > 100 {
            self.sum -= self.durations.pop_front().unwrap();
        }
    }

    fn average_duration(&self) -> f64 {
        self.sum as f64 / self.durations.len() as f64
    }
}

/// Helper to poll a subscriber and collect some metrics.
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
    ) -> Self {
        let subscriber = service
            .subscriber_builder()
            .buffer_size(buffer_size)
            .create()
            .unwrap();
        Self {
            subscriber,
            flight_stats: DurationStats::new(),
            poll_duration_stats: DurationStats::new(),
            total_samples: 0,
            name,
        }
    }

    pub fn poll(&mut self, process_sample: impl Fn(T)) {
        while let Some(sample) = self.subscriber.receive().unwrap() {
            process_sample(*sample);
        }
    }
}

impl<T: std::fmt::Debug + WithCreationTime + ZeroCopySend + Copy> SubscriberPoller<T> {
    pub fn poll_with_metrics(&mut self, process_sample: impl Fn(T)) {
        let start = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc());
        self.total_samples += 1;
        while let Some(sample) = self.subscriber.receive().unwrap() {
            let now = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc());
            let delta = now - sample.creation_time_us();
            self.flight_stats.add_duration(delta);
            process_sample(*sample);
        }
        let delta = offset_datetime_to_timestamp_us(OffsetDateTime::now_utc()) - start;
        self.poll_duration_stats.add_duration(delta);
        if self.total_samples % 100 == 0 {
            info!(
                name = self.name,
                avg_flight_time_us = self.flight_stats.average_duration(),
                avg_poll_time_us = self.poll_duration_stats.average_duration(),
                "Polling stats",
            );
        }
    }
}
