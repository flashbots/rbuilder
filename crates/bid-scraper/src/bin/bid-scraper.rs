use bid_scraper::{bid_sender::NNGBidSender, config::Config};
use rbuilder_config::{load_toml_config, LoggerConfig};
use runng::Listen;
use std::{env, sync::Arc};
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Man, it's not that hard. It's a single parameter: the config file name. Something like:\n{} /home/cool_user_name/some_dir_to_keep_things_nice/some_other_dir_since_im_OCD/another_one/why_are_you_stil_reading_this_question_mark/stop_reading_and_fix_your_command_line/config_file.toml",args[0]);
        return Ok(());
    }

    let config: Config = load_toml_config(args[1].clone())?;

    let logger_config = LoggerConfig {
        env_filter: config.log_level.clone(),
        log_json: config.log_json,
        log_color: config.log_color,
    };
    logger_config.init_tracing()?;

    let global_cancel = CancellationToken::new();
    let global_cancel_clone = global_cancel.clone();
    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        global_cancel_clone.cancel()
    });

    let runng_factory = runng::factory::latest::ProtocolFactory::default();
    let mut nng_publisher_socket = runng_factory
        .publisher_open()
        .expect("unable to create NNG publisher");
    nng_publisher_socket
        .listen(&config.publisher_url)
        .expect("unable to have the NNG publisher listen");

    let sender = Arc::new(NNGBidSender::new(nng_publisher_socket));
    bid_scraper::bid_scraper::run(config.publishers, sender, global_cancel);
    ctrlc.await.unwrap_or_default();
    Ok(())
}
