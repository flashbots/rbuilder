use crate::args::OpRbuilderArgs;
use eyre::Result;
use opentelemetry::{trace::TracerProvider, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use opentelemetry_semantic_conventions::{
    resource::{SERVICE_NAME, SERVICE_VERSION},
    SCHEMA_URL,
};
use reth_optimism_cli::chainspec::OpChainSpecParser;
use reth_optimism_cli::commands::Commands;
use tracing::Level;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Default)]
pub struct TelemetryControl {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryControl {
    pub fn init_from_commands(
        commands: &Commands<OpChainSpecParser, OpRbuilderArgs>,
    ) -> Result<Self> {
        if let Commands::Node(command) = commands {
            Self::init_with_args(&command.ext)
        } else {
            Ok(Self::default())
        }
    }

    pub fn init_with_args(args: &OpRbuilderArgs) -> Result<Self> {
        if args.tracing {
            Self::init(&args.tracing_endpoint)
        } else {
            Ok(Self::default())
        }
    }

    /// Initialize OpenTelemetry tracing with OTLP exporter
    pub fn init(tracing_endpoint: &str) -> Result<Self> {
        tracing::info!("Initialize OTLP");
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(tracing_endpoint)
            .build()?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_resource(resource())
            .with_simple_exporter(exporter)
            .build();

        // opentelemetry::global::set_tracer_provider(provider.clone());
        let tracer = provider.tracer("tracing-otel-subscriber");
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::from_level(
                Level::INFO,
            ))
            .with(tracing_subscriber::fmt::layer())
            .with(OpenTelemetryLayer::new(tracer))
            .try_init()?;

        Ok(Self {
            provider: Some(provider),
        })
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(provider) = self.provider.take() {
            provider.shutdown()?;
        }
        Ok(())
    }
}

fn resource() -> Resource {
    Resource::builder()
        .with_schema_url(
            [
                KeyValue::new(SERVICE_NAME, env!("CARGO_PKG_NAME")),
                KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
            ],
            SCHEMA_URL,
        )
        .build()
}
