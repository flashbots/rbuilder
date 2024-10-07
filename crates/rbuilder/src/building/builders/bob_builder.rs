use ahash::HashMap;
use crate::{
    live_builder::block_output::block_router::UnfinishedBlockRouter,
    building::builders::{BlockBuildingHelper},
    primitives::{Order},
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub async fn run_bob_builder(
    block_router: Arc<UnfinishedBlockRouter>,
    mut order_channel: mpsc::UnboundedReceiver<(Order, Uuid)>,
    mut block_channel: mpsc::UnboundedReceiver<(Box<dyn BlockBuildingHelper>, Uuid)>,
    cancel: CancellationToken
) {
    let mut block_cache = HashMap::<Uuid, Box<dyn BlockBuildingHelper>>::default();
    loop {
        tokio::select!{
            _ = cancel.cancelled() => {
                break;
            }
            Some((order, uuid)) = order_channel.recv() => {
                match block_cache.get(&uuid) {
                    Some(block_building_helper) => {
                        let mut new_block = block_building_helper.box_clone();
                        match new_block.commit_raw_order(&order) {
                            Ok(_) => {
                                block_router.new_bob_block(new_block)
                            }
                            Err(_) => {}
                        }
                    }
                    None => {}
                }
            }
            Some((block, uuid)) = block_channel.recv() => {
                block_cache.insert(uuid, block);
            }
        }
    }
}
