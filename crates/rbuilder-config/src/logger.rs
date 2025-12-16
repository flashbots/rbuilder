use tracing_subscriber::{
    fmt::{
        format::{DefaultFields, Format, Json, JsonFields},
        SubscriberBuilder,
    },
    EnvFilter,
};

/// Logger configuration.
#[derive(PartialEq, Eq, Clone, Debug, serde::Deserialize)]
pub struct LoggerConfig {
    pub env_filter: String,
    #[serde(default)]
    pub log_json: bool,
    #[serde(default)]
    pub log_color: bool,
}

impl LoggerConfig {
    /// Default logger configuration for development.
    pub fn dev() -> Self {
        Self {
            env_filter: String::from("info"),
            log_color: true,
            log_json: false,
        }
    }
}

impl LoggerConfig {
    /// Create an [`EnvFilter`] from the configuration. Fails if the filter
    /// string is invalid.
    pub(crate) fn filter(&self) -> eyre::Result<EnvFilter> {
        EnvFilter::try_new(&self.env_filter).map_err(|e| eyre::eyre!(e))
    }

    /// Create a [`SubscriberBuilder`] from the configuration. Fails if the
    /// filter string is invalid.
    pub(crate) fn builder(
        &self,
    ) -> eyre::Result<SubscriberBuilder<DefaultFields, Format, EnvFilter>> {
        self.filter()
            .map(|filter| tracing_subscriber::fmt().with_env_filter(filter))
    }

    /// Create a JSON-formatting [`SubscriberBuilder`] from the configuration.
    ///
    /// Errors if the filter string is invalid.
    ///
    /// # Panics
    ///
    /// If `self.log_json` is false.
    pub(crate) fn json(
        &self,
    ) -> eyre::Result<SubscriberBuilder<JsonFields, Format<Json>, EnvFilter>> {
        assert!(self.log_json);
        self.builder().map(SubscriberBuilder::json)
    }

    /// Create an ANSI-formatting [`SubscriberBuilder`] from the configuration.
    ///
    /// Errors if the filter string is invalid.
    ///
    /// # Panics
    ///
    /// If `self.log_json` is true.
    pub(crate) fn ansi(&self) -> eyre::Result<SubscriberBuilder<DefaultFields, Format, EnvFilter>> {
        assert!(!self.log_json);
        self.builder().map(|b| b.with_ansi(self.log_color))
    }

    /// Initialize tracing subscriber based on the configuration.
    #[deprecated(
        note = "This function will not configure OTLP tracing, which may be desirable. Instead, use the crate::tracing::TracingConfig to initialize logging and/or tracing."
    )]
    pub fn init_tracing(self) -> eyre::Result<()> {
        if self.log_json {
            self.json()?.try_init()
        } else {
            self.ansi()?.try_init()
        }
        .map_err(|err| eyre::eyre!(err))
    }
}
