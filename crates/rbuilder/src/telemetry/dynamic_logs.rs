//! This module provides a functionality to dynamically change the log level

use lazy_static::lazy_static;
use std::{
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{
    filter::Filtered, fmt, layer::SubscriberExt, reload, reload::Handle, util::SubscriberInitExt,
    EnvFilter, Layer, Registry,
};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;
type FilteredLayer = Filtered<BoxedLayer, EnvFilter, Registry>;

lazy_static! {
    static ref DEFAULT_CONFIG: Arc<Mutex<LoggerConfig>> =
        Arc::new(Mutex::new(LoggerConfig::default()));
    static ref RELOAD_HANDLE: Arc<Mutex<Option<Handle<FilteredLayer, Registry>>>> =
        Arc::new(Mutex::new(None));
}

#[derive(Debug, Clone)]
pub struct LoggerConfig {
    pub env_filter: String,
    pub file: Option<PathBuf>,
    pub log_json: bool,
    pub log_color: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            env_filter: "info".to_string(),
            file: None,
            log_json: false,
            log_color: true,
        }
    }
}

pub fn default_log_config() -> LoggerConfig {
    DEFAULT_CONFIG.lock().unwrap().clone()
}

/// Reloads the log layer with the provided config
pub fn set_log_config(config: LoggerConfig) -> eyre::Result<()> {
    let (env, write_layer) = create_filter_and_write_layer(&config)?;
    let handle = RELOAD_HANDLE.lock().unwrap();
    handle
        .as_ref()
        .ok_or_else(|| eyre::eyre!("tracing subscriber is not set up"))?
        .modify(|layer| {
            *layer.filter_mut() = env;
            *layer.inner_mut() = write_layer;
        })?;
    Ok(())
}

/// Resets the log layer to the default value that was set up during the initialization of the tracing subscriber
pub fn reset_log_config() -> eyre::Result<()> {
    let default_filter = default_log_config();
    set_log_config(default_filter)?;
    Ok(())
}

/// Sets up the tracing subscriber with the provided filter
/// To reload env filter, use `set_env_filter` and `reset_env_filter` functions
pub fn setup_reloadable_tracing_subscriber(config: LoggerConfig) -> eyre::Result<()> {
    {
        let mut default_config = DEFAULT_CONFIG.lock().unwrap();
        *default_config = config.clone();
    }

    let (env, layer) = create_filter_and_write_layer(&config)?;
    let layer = layer.with_filter(env);
    let (reload_layer, reload_handle) = reload::Layer::new(layer);

    {
        let mut handle = RELOAD_HANDLE.lock().unwrap();
        *handle = Some(reload_handle);
    }
    tracing_subscriber::registry().with(reload_layer).init();

    Ok(())
}

fn create_filter_and_write_layer(config: &LoggerConfig) -> eyre::Result<(EnvFilter, BoxedLayer)> {
    let env = EnvFilter::try_new(&config.env_filter)?;
    let layer = fmt::Layer::default().with_ansi(config.log_color);
    let layer = if let Some(file) = &config.file {
        let file = Arc::new(File::create(file)?);
        if config.log_json {
            layer.json().with_writer(file).boxed()
        } else {
            layer.with_writer(file).boxed()
        }
    } else if config.log_json {
        layer.json().boxed()
    } else {
        layer.boxed()
    };

    Ok((env, layer))
}
