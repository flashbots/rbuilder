use crate::building::builders::{BlockBuildingAlgorithm, BlockBuildingAlgorithmInput};
use reth_db::database::Database;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, trace};

#[derive(Debug)]
pub struct BlockBuildingPool<DB> {
    builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB>>>,
}

impl<DB: Database + Clone + 'static> BlockBuildingPool<DB> {
    pub fn new(builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB>>>) -> Self {
        BlockBuildingPool { builders }
    }
}

impl<DB: Database + std::fmt::Debug + Clone + 'static> BlockBuildingAlgorithm<DB>
    for BlockBuildingPool<DB>
{
    fn name(&self) -> String {
        "BlockBuildingPool".to_string()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB>) {
        let (broadcast_input, _) = broadcast::channel(10_000);
        let block_number = input.ctx.block_env.number.to::<u64>();

        for builder in self.builders.iter() {
            let builder_name = builder.name();
            debug!(block = block_number, builder_name, "Spawning builder job");

            let builder = builder.clone();
            let input = BlockBuildingAlgorithmInput {
                provider_factory: input.provider_factory.clone(),
                ctx: input.ctx.clone(),
                input: broadcast_input.subscribe(),
                sink: input.sink.clone(),
                cancel: input.cancel.clone(),
            };

            tokio::task::spawn_blocking(move || {
                builder.build_blocks(input);
                debug!(block = block_number, builder_name, "Stopped builder job");
            });
        }

        tokio::spawn(multiplex_job(input.input, broadcast_input));
    }
}

async fn multiplex_job<T: Clone>(mut input: broadcast::Receiver<T>, sender: broadcast::Sender<T>) {
    // we don't worry about waiting for input forever because it will be closed by producer job
    while let Ok(input) = input.recv().await {
        // we don't create new subscribers to the broadcast so here we can be sure that err means end of receivers
        if sender.send(input).is_err() {
            return;
        }
    }
    trace!("Cancelling multiplex job");
}
