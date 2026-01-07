//!Simple bidding service using NewTrueBlockValueBiddingService.

use std::env;

use rbuilder::live_builder::{
    block_output::{
        bidding_service_interface::{LandedBlockInfo, RelaySet},
        true_value_bidding_service::NewTrueBlockValueBiddingService,
    },
    config::{
        parse_true_block_value_bidding_service_config, TrueBlockValueBiddingServiceConfigToml,
    },
};

use rbuilder_config::{load_toml_config, EnvOrValue, LoggerConfig};
use rbuilder_operator::{
    bidding_service_wrapper::server::{
        bidding_service::run_bidding_service,
        bidding_service_server_initializer::BiddingServiceFactoryError,
        bidding_service_with_reload::NotReloadingBiddingServiceWithReload,
    },
    build_info::rbuilder_version,
};

use rbuilder_primitives::mev_boost::MevBoostRelayID;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ipc_path: String,
    pub log_json: bool,
    pub log_level: EnvOrValue<String>,
    pub log_color: bool,
    #[serde(flatten)]
    pub true_block_value_bidding_service_config: TrueBlockValueBiddingServiceConfigToml,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Man, it's not that hard. It's a single parameter: the config file name. Something like:\n{} /home/cool_user_name/some_dir_to_keep_things_nice/some_other_dir_since_im_OCD/another_one/why_are_you_stil_reading_this_question_mark/stop_reading_and_fix_your_command_line/config_file.toml",args[0]);
        return Ok(());
    }
    let config = load_toml_config::<Config>(args[1].clone())?;

    let log_config = LoggerConfig {
        env_filter: config.log_level.value()?,
        log_json: config.log_json,
        log_color: config.log_color,
    };
    log_config.init_tracing()?;

    let parsed_config = parse_true_block_value_bidding_service_config(
        &config.true_block_value_bidding_service_config,
    )?;
    let bidding_service_factory_factory = move |_landed_block_info: Vec<LandedBlockInfo>,all_relays: &[MevBoostRelayID]|
              -> Result<
            Box<dyn rbuilder_operator::bidding_service_wrapper::server::bidding_service_with_reload::BiddingServiceWithReload>,
            BiddingServiceFactoryError,
        > {
            let bidding_service = NewTrueBlockValueBiddingService::new(&parsed_config, RelaySet::new(all_relays.to_vec())).
                map_err(|_| BiddingServiceFactoryError::UnableToCreateBiddingService)?;
            Ok(Box::new(NotReloadingBiddingServiceWithReload::new(Box::new(bidding_service))))
        };
    let version = rbuilder_version();
    let version_info = rbuilder_operator::bidding_service_wrapper::BidderVersionInfo {
        git_commit: version.git_commit,
        git_ref: version.git_ref,
        build_time_utc: version.build_time_utc,
    };
    run_bidding_service(
        config.ipc_path.clone(),
        Box::new(bidding_service_factory_factory),
        Some(version_info),
    )
    .await
}

#[cfg(test)]
mod test {
    use crate::Config;
    use rbuilder_config::load_toml_config;
    use std::path::PathBuf;

    #[test]
    fn test_parse_config() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../examples/config/rbuilder-operator/config-tbv-bidding-service.toml");
        let _ = load_toml_config::<Config>(p).expect("Config load");
    }
}
