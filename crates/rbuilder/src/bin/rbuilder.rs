//! Instantiation of cli::run on our sample configuration.
//! This runs the default included rbuilder!

use rbuilder::{
    live_builder::{cli, config::Config},
    utils::build_info::print_version_info,
};
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    return cli::run::<Config>(print_version_info, None).await;
}
