This library is useful when you need to calculate Ethereum root hash many times on top of the same parent block using reth database.

To use this, for each parent block:
* create `SparseTrieSharedCache`
* call `calculate_root_hash_with_sparse_trie` with the given cache, reth db view and execution outcome.


### Speedup example.

* block 20821340
* machine with 64 cores, Samsung 980Pro SSD

We calculate root hash of some specific blocks in a loop using the same changes.
This implementation caches only disk access, all storage and main trie hashes are calculated fully on each iteration.

```
reth parallel root hash:

first iteration : 220 ms
next iterations: 140 ms (median, stable)

eth-sparse-mpt:

first iteration : 225 ms
next iterations: 5.1 ms (median, stable)
```
