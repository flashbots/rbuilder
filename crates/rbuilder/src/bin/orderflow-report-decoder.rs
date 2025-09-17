use std::path::PathBuf;

use clap::Parser;
use rbuilder::live_builder::order_flow_tracing::events::{
    ReplaceableOrderEventWithTimestamp, SimulationEventWithTimestamp,
};
use rbuilder::live_builder::order_flow_tracing::report_serialization::load_report;
use time::OffsetDateTime;

#[derive(Parser, Debug, Clone)]
struct Cli {
    path: PathBuf,
}

enum Event {
    SimulationEvent(SimulationEventWithTimestamp),
    ReplaceableOrderEvent(ReplaceableOrderEventWithTimestamp),
}

impl Event {
    fn timestamp(&self) -> OffsetDateTime {
        match self {
            Event::SimulationEvent(e) => e.timestamp,
            Event::ReplaceableOrderEvent(e) => e.timestamp,
        }
    }
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    let report = load_report(cli.path)?;
    println!("{:?}", report.sim_events.len());
    let mut events: Vec<Event> = report
        .sim_events
        .into_iter()
        .map(Event::SimulationEvent)
        .chain(
            report
                .order_input_events
                .into_iter()
                .map(Event::ReplaceableOrderEvent),
        )
        .collect();
    events.sort_by_key(|e| e.timestamp());

    for event in events {
        match event {
            Event::SimulationEvent(e) => println!("SimulationEvent: {e:?}"),
            Event::ReplaceableOrderEvent(e) => println!("ReplaceableOrderEvent: {e:?}"),
        }
    }
    Ok(())
}
