use opentelemetry::{trace::TracerProvider, KeyValue};
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use opentelemetry_semantic_conventions::{
    resource::{DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME, SERVICE_VERSION},
    SCHEMA_URL,
};
use tracing_subscriber::Layer;

/// Drop guard for the OTLP exporter. This will shutdown the exporter when
/// dropped, and generally should be held for the lifetime of the `main`
/// function.
///
/// The guard may be used to produce a [`tracing`] layer that can be added to
/// an existing [`tracing::Subscriber`].
///
/// ```
/// # use rbuilder::utils::otlp::{OtelConfig, OtlpGuard};
/// # fn test() {
/// fn main() {
///     let cfg = OtelConfig::from_env().unwrap();
///     let guard = cfg.provider();
///     // do stuff
///     // drop the guard when the program is done
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct OtlpGuard(SdkTracerProvider);

impl OtlpGuard {
    /// Get a tracer from the provider.
    fn tracer(&self, s: &'static str) -> opentelemetry_sdk::trace::Tracer {
        self.0.tracer(s)
    }

    /// Create a filtered tracing layer.
    pub fn layer<S>(&self) -> impl Layer<S>
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        let tracer = self.tracer("tracing-otel-subscriber");
        tracing_opentelemetry::layer().with_tracer(tracer)
    }
}

/// Configuration for the OTLP system. Currently this allows configuring the
/// deployment environment name.
///
/// OTEL and OTLP are configured primarily via environment variables. The
/// standard variables are as follows:
///
/// - `OTEL_TRACES_EXPORTER` - Set the exporter to be used. Generally should be
///   set to `otlp`. Other options include `none`, `zipkin`, etc. See the
///   [relevant documentation] for more details.
///
/// - `OTEL_EXPORTER_OTLP_ENDPOINT` - Set to the URL endpoint to which to send
///   OTLP data. If not set, traces will not be exported. This will typically
///   be a URL ending in port 4317 (grpc) or 4318 (http).
///
/// - `OTEL_EXPORTER_OTLP_PROTOCOL` - Specifies the OTLP transport protocol to
///   be used for all telemetry data. Acceptable values are `grpc`,
///   `http/protobuf`, and `http/json`.
///
/// For advanced exporter configuration via environment variables, see the
/// [OTLP documentation].
///
/// [relevant documentation]: https://opentelemetry.io/docs/instrumentation/js/exporters/#environment-variables
/// [OTLP documentation]: https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/
#[derive(Default, PartialEq, Eq, Clone, Debug, serde::Deserialize)]
#[non_exhaustive]
pub struct OtlpConfig {
    /// The name of the deployment environment (a.k.a deployment tier), e.g.
    /// "production", "staging". [See documentation here.]
    ///
    /// [See documentation here.]: https://opentelemetry.io/docs/specs/semconv/resource/deployment-environment/
    pub otlp_environment: String,
}

impl OtlpConfig {
    /// Default OTEL configuration for development.
    pub fn dev() -> Self {
        Self {
            otlp_environment: "development".to_owned(),
        }
    }

    /// Instantiate a new OTEL configuration, with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the environment name.
    pub fn with_environment(mut self, name: impl Into<String>) -> Self {
        self.otlp_environment = name.into();
        self
    }

    fn resource(&self) -> Resource {
        Resource::builder()
            .with_schema_url(
                [
                    KeyValue::new(SERVICE_NAME, env!("CARGO_PKG_NAME")),
                    KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
                    KeyValue::new(DEPLOYMENT_ENVIRONMENT_NAME, self.otlp_environment.clone()),
                ],
                SCHEMA_URL,
            )
            .build()
    }

    /// Instantiate a new OTEL provider, with a simple batch exporter, and
    /// start relevant tasks. Return a guard that will shut down the provider
    /// and exporter when dropped.
    pub fn provider(&self) -> OtlpGuard {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .unwrap();

        let provider = SdkTracerProvider::builder()
            // Customize sampling strategy
            // If export trace to AWS X-Ray, you can use XrayIdGenerator
            .with_resource(self.resource())
            .with_batch_exporter(exporter)
            .build();

        OtlpGuard(provider)
    }

    /// Create a new OTEL provider, returning both the guard and a tracing
    /// layer that can be added to a subscriber.
    pub fn into_guard_and_layer<S>(self) -> (OtlpGuard, impl Layer<S>)
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        let guard = self.provider();
        let layer = guard.layer();
        (guard, layer)
    }
}
