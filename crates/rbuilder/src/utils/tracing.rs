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
