/// Allows to call event! with level as a parameter (event! only allows constants as level parameter)
#[macro_export]
macro_rules! dynamic_event {
    ($level:expr, $($arg:tt)+) => {
        match $level {
            Level::TRACE => event!(Level::TRACE, $($arg)+),
            Level::DEBUG => event!(Level::DEBUG, $($arg)+),
            Level::INFO => event!(Level::INFO, $($arg)+),
            Level::WARN => event!(Level::WARN, $($arg)+),
            Level::ERROR => event!(Level::ERROR, $($arg)+),
        }
    };
}

pub use dynamic_event;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone)]
pub struct LoggerConfig {
    pub env_filter: String,
    pub log_json: bool,
    pub log_color: bool,
}

pub fn setup_tracing_subscriber(config: LoggerConfig) -> eyre::Result<()> {
    let env = EnvFilter::try_new(&config.env_filter)?;
    if config.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env)
            .try_init()
            .map_err(|err| eyre::format_err!("{}", err))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env)
            .with_ansi(config.log_color)
            .try_init()
            .map_err(|err| eyre::format_err!("{}", err))?;
    }
    Ok(())
}
