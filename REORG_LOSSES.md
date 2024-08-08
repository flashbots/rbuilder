# Reorg losses

From time to time ethereum network forks for a short time causing block reorgs.
When a reorg happens all the txs from the losing fork goes to the mempool. This includes any private transaction, particularly any builder generated tx like the one we add at the end of the block to pay to the validator.

If this happens, the builder ends up paying the bid to a random validator without winning any block! Notice that **this happens even if this tx gives not tip to the block**, probably because some builders are such good guys that when all paying transactions are included in the block they are building (and some gas is left) they include free txs.

This is a rare event but happens from time to time and **causes losses to the builder**. As soon as the builder wins a real (not-reorged) block the nonce changes, the old reorged tx gets invalidated and we are safe again. 

As far as we know, main builders are not doing anything about this.
These are some examples of this events:
- Flashbots: https://etherscan.io/tx/0xdb876101f649bf6801a04ec9da5535449eaec32247a59735bc3c88c287b1d5a9
- Beaver: https://etherscan.io/tx/0xaef73164468705b14f830eda7461b10838fe7f575bec8e0169b1cab6906ed0f3
- Titan: https://etherscan.io/tx/0x936ceee44b270b52d2765c78f27e3b4a9e067ef4dca41ce4e699664fde6c300d

The main 2 ideas we are considering are:
- Pay through a contract that checks that coinbase is the builder address and reverts otherwise. A gas analysis needs to be done to check how much extra cost this adds to regular blocks. An alternative is to do this only when the pay tx is too big.
- Detect reorgs and post a tx with the same nonce paying the smallest possible gas tip trying to lure the builders into using this tx and not the reorged.
- Periodically remove the winnings from the builder account to limit possible losses.
