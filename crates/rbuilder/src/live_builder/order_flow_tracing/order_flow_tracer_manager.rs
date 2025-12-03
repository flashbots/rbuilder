use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use tracing::error;

use super::order_flow_tracer::OrderFlowTracer;
use crate::live_builder::{
    block_output::bidding_service_interface::SlotBlockId,
    order_flow_tracing::report_serialization::save_report,
    order_input::replaceable_order_sink::ReplaceableOrderSink,
    simulation::simulation_job_tracer::{NullSimulationJobTracer, SimulationJobTracer},
};

pub trait OrderFlowTracerManager: std::fmt::Debug + Sync + Send {
    /// Takes the destination ReplaceableOrderSink and returns the one that will trace the events.
    /// Also returns the SimulationJobTracer for this block to be given to the simulation stage.
    /// This method is called should be called at the beginning of each block and it might perform some resource intensive operations (fluishig to disk, etc) since
    /// we assume at this moment we are not resource constrained.
    fn create_tracers(
        &mut self,
        id: SlotBlockId,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> (Arc<dyn SimulationJobTracer>, Box<dyn ReplaceableOrderSink>);
}

#[derive(Debug)]
pub struct NullOrderFlowTracerManager {}

impl OrderFlowTracerManager for NullOrderFlowTracerManager {
    fn create_tracers(
        &mut self,
        _id: SlotBlockId,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> (Arc<dyn SimulationJobTracer>, Box<dyn ReplaceableOrderSink>) {
        (Arc::new(NullSimulationJobTracer {}), sink)
    }
}

/// Every tracer should be flushed in the next block so anything above 2 covers delays and forks.
const MAX_ACTIVE_TRACERS: usize = 10;

/// Real OrderFlowTracerManager that creates and OrderFlowTracer for each block and when the system is done using it
/// it saves the traces to disk keeping only the last N traces in disk.
#[derive(Debug)]
pub struct OrderFlowTracerManagerImpl {
    /// Every created tracer is stored here and when the Arc refcount reaches 1 we "steal" it and save it to disk.
    /// We could have added wrappers with notifications on drop but it's not worth the complexity (or is it that I am lazy?).
    active_tracers: VecDeque<Arc<OrderFlowTracer>>,
    /// Path to the directory where the tracers will be saved.
    storage_path: PathBuf,
    /// Content of storage_path.
    /// Initially sorted by name and then pushing the new ones.
    storage_blocks: VecDeque<PathBuf>,
    /// Max number of blocks to keep in disk (storage_blocks.len()).
    /// If we have more than this number we remove (from disk!) the oldest ones (storage_blocks.pop_front()).
    max_blocks_to_keep: usize,
}

impl OrderFlowTracerManagerImpl {
    pub fn new(storage_path: PathBuf, max_blocks_to_keep: usize) -> Result<Self, eyre::Error> {
        let storage_blocks_read_dir = std::fs::read_dir(storage_path.clone())?;
        let mut storage_blocks = Vec::new();
        for block in storage_blocks_read_dir {
            let entry = block?;
            let path = entry.path();
            storage_blocks.push(path);
        }
        storage_blocks.sort();
        Ok(Self {
            active_tracers: VecDeque::new(),
            storage_path,
            storage_blocks: VecDeque::from(storage_blocks),
            max_blocks_to_keep,
        })
    }

    fn filename_for(id: SlotBlockId) -> String {
        format!("{}-{:?}.bin", id.block, id.parent_block_hash)
    }
    fn flush_tracers(&mut self) {
        let mut index = 0;
        // can't use standard iter since we need to modify the deque.
        while index < self.active_tracers.len() {
            let tracer_ref = &self.active_tracers[index];
            if Arc::strong_count(tracer_ref) == 1 {
                let tracer = self.active_tracers.remove(index).unwrap(); // Safe since index < self.active_tracers.len()
                if let Ok(tracer) = Arc::try_unwrap(tracer) {
                    self.save_tracer_to_disk(tracer);
                } else {
                    error!("Failed to unwrap tracer");
                }
            } else {
                index += 1;
            }
        }

        // Any not flushed tracer above MAX_ACTIVE_TRACERS goes away :(
        // This is typically a bug in the system.
        while self.active_tracers.len() > MAX_ACTIVE_TRACERS {
            let _ = self.active_tracers.pop_front();
        }
        self.delete_old_block_files();
    }

    fn save_tracer_to_disk(&mut self, tracer: OrderFlowTracer) {
        let block_path = self.storage_path.join(Self::filename_for(tracer.id()));
        let report = tracer.into_report();
        if let Err(error) = save_report(report, block_path.clone()) {
            error!(?error, ?block_path, "Failed to save report to disk");
            return;
        }
        self.storage_blocks.push_back(block_path);
    }

    fn delete_old_block_files(&mut self) {
        while self.storage_blocks.len() > self.max_blocks_to_keep {
            let block_path = self.storage_blocks.pop_front();
            if let Some(block_path) = block_path {
                if let Err(error) = std::fs::remove_file(block_path.clone()) {
                    error!(?error, ?block_path, "Failed to delete block file");
                }
            }
        }
    }
}

impl OrderFlowTracerManager for OrderFlowTracerManagerImpl {
    fn create_tracers(
        &mut self,
        id: SlotBlockId,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> (Arc<dyn SimulationJobTracer>, Box<dyn ReplaceableOrderSink>) {
        self.flush_tracers();
        let (tracer, sink) = OrderFlowTracer::new(id, sink);
        self.active_tracers.push_back(tracer.clone());
        (tracer, sink)
    }
}
#[cfg(test)]
mod tests {

    use std::fs::File;

    use alloy_primitives::BlockHash;
    use tempfile::TempDir;

    use crate::live_builder::order_input::replaceable_order_sink::NullReplaceableOrderSink;

    use super::*;

    fn file_count(dir: PathBuf) -> usize {
        let entries = std::fs::read_dir(dir).unwrap();
        entries.count()
    }
    struct TestContext {
        tracers: VecDeque<Arc<dyn SimulationJobTracer>>,
        manager: OrderFlowTracerManagerImpl,
        next_block: u64,
    }

    impl TestContext {
        fn new(manager: OrderFlowTracerManagerImpl, next_block: u64) -> Self {
            Self {
                tracers: VecDeque::new(),
                manager,
                next_block,
            }
        }

        fn add_tracer(&mut self) {
            let (tracer, _) = self.manager.create_tracers(
                SlotBlockId::new(0, self.next_block, BlockHash::ZERO),
                Box::new(NullReplaceableOrderSink {}),
            );
            self.next_block += 1;
            self.tracers.push_back(tracer);
        }
    }

    #[test]
    fn files_deleted_correctly() {
        let temp_dir = TempDir::new().unwrap();
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 0);
        let max_files = 5;
        let manager =
            OrderFlowTracerManagerImpl::new(temp_dir.path().to_path_buf(), max_files).unwrap();
        let mut test_context = TestContext::new(manager, 0);
        test_context.add_tracer();
        // Nothing should be in disk since we just created a tracer.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 0);
        test_context.add_tracer();
        // Nothing should be in disk since we are still using the tracers
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 0);
        // 1 tracer dropped
        test_context.tracers.pop_front();
        test_context.add_tracer();
        // When creating this new tracer we should have 1 file in disk since we dropped a tracer before that.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 1);
        // 2 tracers dropped
        test_context.tracers.clear();
        test_context.add_tracer();
        // 3 tracers dropped in total.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 3);
        test_context.add_tracer();
        test_context.add_tracer();
        test_context.add_tracer();
        // Nothing dropped, still 3 files in disk.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 3);
        // 4 were dropped
        test_context.tracers.clear();
        test_context.add_tracer();
        // We dropped more than max_files so we should have max_files files in disk.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), max_files);
    }

    /// Checks that deletes files from previous runs.
    #[test]
    fn old_files_deleted_correctly() {
        let temp_dir = TempDir::new().unwrap();
        let oldest_path = temp_dir
            .path()
            .join(OrderFlowTracerManagerImpl::filename_for(SlotBlockId::new(
                0,
                0,
                BlockHash::ZERO,
            )));
        {
            let _file = File::create(oldest_path.clone()).unwrap();
        };
        assert_eq!(file_count(temp_dir.path().to_path_buf()), 1);
        let max_files = 5;
        let manager =
            OrderFlowTracerManagerImpl::new(temp_dir.path().to_path_buf(), max_files).unwrap();
        let mut test_context = TestContext::new(manager, 1);
        test_context.add_tracer();
        test_context.add_tracer();
        test_context.add_tracer();
        test_context.add_tracer();
        test_context.add_tracer();
        test_context.tracers.clear();
        test_context.add_tracer();
        // 5 new files were created.
        assert_eq!(file_count(temp_dir.path().to_path_buf()), max_files);
        assert!(!oldest_path.exists());
    }
}
