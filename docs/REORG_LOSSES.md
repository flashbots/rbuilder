# Reorg losses


From time to time, the Ethereum network forks for a short time (you can check that at https://etherscan.io/blocks_forked), causing block reorganizations (reorgs). When a reorg happens, all the transactions from the losing fork go to the mempool. This includes any private transactions, particularly any builder-generated transactions, such as the ones we add at the end of the block to pay the validator.


If this happens, the builder ends up paying the bid to a random validator without winning a block! Notice that **this happens even if this transaction gives no tip to the block**, probably because, for some builders, when all paying transactions are included in the block they are building (and some gas is left), they include free transactions.


This is a rare event but occurs from time to time and **causes losses to the builder**. As soon as the builder wins a real (non-reorged) block, the nonce changes, the old reorged transaction gets invalidated, and we are safe again.


As far as we know, major builders are not doing anything about this. Here are some examples of these events:
- Flashbots: https://etherscan.io/tx/0xdb876101f649bf6801a04ec9da5535449eaec32247a59735bc3c88c287b1d5a9
- Beaver: https://etherscan.io/tx/0xaef73164468705b14f830eda7461b10838fe7f575bec8e0169b1cab6906ed0f3
- Titan: https://etherscan.io/tx/0x936ceee44b270b52d2765c78f27e3b4a9e067ef4dca41ce4e699664fde6c300d

The main two ideas we are considering are:
- Pay through a contract that checks that the coinbase is the builder's address and reverts otherwise. A gas analysis needs to be done to determine how much extra cost this adds to regular blocks. An alternative is to implement this only when the payment transaction is too large.
- Detect reorgs and post a transaction with the same nonce, paying the smallest possible gas tip, in an attempt to lure the builders into using this transaction instead of the reorged one.
- Periodically remove the winnings from the builder's account to limit potential losses.
