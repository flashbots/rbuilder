use tracing_subscriber::EnvFilter;

/// Logger configuration.
#[derive(Debug, Clone)]
pub struct LoggerConfig {
    pub env_filter: String,
    pub log_json: bool,
    pub log_color: bool,
}

impl LoggerConfig {
    /// Initialize tracing subscriber based on the configuration.
    pub fn init_tracing(self) -> eyre::Result<()> {
        let env_filter = EnvFilter::try_new(&self.env_filter)?;
        let builder = tracing_subscriber::fmt().with_env_filter(env_filter);
        let result = if self.log_json {
            builder.json().try_init()
        } else {
            builder.with_ansi(self.log_color).try_init()
        };
        result.map_err(|err| eyre::format_err!("{err}"))
    }
}
