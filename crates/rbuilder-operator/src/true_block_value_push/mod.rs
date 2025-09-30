//! This module handles the push of the best block (true block value) to a redis channel.
//! This information is used by the smart-multiplexing core to decide when to stop multiplexing order flow.
//! We use a redis channel for historical reasons but it could be changed to a direct streaming.
//! Could be improved but this is just a refactoring resuscitating the old code.

pub mod best_true_value_observer;
pub mod best_true_value_pusher;
mod blocks_processor_backend;
mod redis_backend;
