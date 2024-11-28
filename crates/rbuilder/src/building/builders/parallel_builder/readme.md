# Parallel Builder
The parallel builder is a block building algorithm that runs key components of building in parallel and attempts to do more sophisticated merging of bundles.

It's primary differentiator is that it identifies groups of conflicting orders and resolves them independently of each other and in parallel. By doing so, we can pipeline the stages of orderflow intake, conflict resolution, and building a final block.

## Components and Process Flow
1. The **[OrderIntakeStore](order_intake_store.rs)** consumes orders from the intake store.
2. The **[ConflictFinder](groups.rs)** identifies conflict groups among the orders.
3. The **[ConflictTaskGenerator](task.rs)** creates tasks for resolving conflicts.
4. The **[ConflictResolvingPool](conflict_resolving_pool.rs)** is a pool of threads that process these tasks in parallel, executing merging algorithms defined by tasks.
5. The **[ResultsAggregator](results_aggregator.rs)** collects the results of conflict resolution, keeping track of the best results.
6. The **[BlockBuildingResultAssembler](block_building_result_assembler.rs)** constructs blocks from the best results obtained from the `ResultsAggregator`.

## Usage live
The parallel builder requires extra configuration options to be used live. Here is an example for your config file:

```
[[builders]]
name = "parallel"
algo = "parallel-builder"
discard_txs = true
num_threads = 25
```

## Backtesting
The parallel builder can be backtested. However, it is quite slow due to how it is currently implemented. This isn't reflective of the latency performance in the live environment and could be improved with more work.