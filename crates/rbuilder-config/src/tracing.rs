use eyre::WrapErr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

use crate::{LoggerConfig, OtlpConfig, OtlpGuard};

/// Unified config object for local logging and distributed tracing via OTLP.
#[derive(PartialEq, Eq, Clone, Debug, serde::Deserialize)]
#[non_exhaustive]
pub struct TracingConfig {
    /// Logger configuration.
    pub logger: LoggerConfig,
    /// OTLP configuration.
    pub otlp: Option<OtlpConfig>,
}

impl From<LoggerConfig> for TracingConfig {
    fn from(logger: LoggerConfig) -> Self {
        Self { logger, otlp: None }
    }
}

/// Install a format layer based on the logger configuration, and then install
/// the registry.
macro_rules! add_fmt_and_install {
    (json @ $registry:ident, $filter:ident) => {{
        let fmt = tracing_subscriber::fmt::layer().json().with_filter($filter);
        $registry.with(fmt).try_init()
    }};
    (log @ $registry:ident, $filter:ident) => {{
        let fmt = tracing_subscriber::fmt::layer().with_filter($filter);
        $registry.with(fmt).try_init()
    }};
    ($registry:ident, $log_cfg:expr) => {{
        let filter = $log_cfg.filter()?;
        if $log_cfg.log_json {
            add_fmt_and_install!(json @ $registry, filter)
        } else {
            add_fmt_and_install!(log @ $registry, filter)
        }.wrap_err("failed to initialize tracing subscriber")
    }};
}

impl TracingConfig {
    /// Default tracing configuration for development. Note that this does not
    /// enable OTLP exporting.
    pub fn dev() -> Self {
        Self {
            logger: crate::logger::LoggerConfig::dev(),
            otlp: None,
        }
    }

    /// Create a new tracing configuration with the given logger config.
    pub fn new(logging: LoggerConfig) -> Self {
        Self {
            logger: logging,
            otlp: None,
        }
    }

    /// Set the logger configuration, dropping any existing configuration.
    pub fn with_logger(mut self, logger: LoggerConfig) -> Self {
        self.logger = logger;
        self
    }

    /// Enable OTLP exporting with the given environment name.
    ///
    /// If `environment` is `None`, the OTLP exporter will be created
    /// with the name `"unknown"`.
    pub fn with_otlp(mut self, otlp: OtlpConfig) -> Self {
        self.otlp = Some(otlp);
        self
    }

    /// Initialize tracing based on the configuration.
    ///
    /// The returned `OtlpGuard`, if any, should be held for the lifetime of
    /// the application to ensure proper flushing of OTLP spans from the
    /// exporter to the OTLP collector.
    ///
    /// ```
    /// # use rbuilder_config::{TracingConfig, LoggingConfig, OtlpConfig};
    /// # fn test() -> eyre::Result<()> {
    /// let config = TracingConfig {
    ///     logger: LoggingConfig::dev(),
    ///     otlp: Some(OtlpConfig::dev()),
    ///     ..
    /// };
    /// // Keep the guard
    /// let _guard = config.init_tracing()?;
    ///
    /// // Application code here
    /// // ...
    ///
    /// # Ok(())
    /// # }
    /// ```
    #[must_use = "The OtelGuard should be held for the lifetime of the application"]
    pub fn init_tracing(self) -> eyre::Result<Option<OtlpGuard>> {
        if self.otlp.is_none() {
            #[allow(deprecated)]
            return self.logger.init_tracing().map(|_| None);
        }

        // This code gets a little ugly because we need to build the tracing
        // subscriber in 2 parts, each of which may have two different types,
        // i.e. it's difficult to DRY up. The outer if let handles whether OTLP
        // is enabled, and the inner macro usage applies the fmt layer based
        // on the logger config.

        let registry = tracing_subscriber::registry();
        let logger = self.logger;

        if let Some((otlp_guard, otlp_layer)) = self.otlp.map(|cfg| cfg.into_guard_and_layer()) {
            let registry = registry.with(otlp_layer);
            add_fmt_and_install!(registry, logger).map(|_| Some(otlp_guard))
        } else {
            add_fmt_and_install!(registry, logger).map(|_| None)
        }
    }
}
