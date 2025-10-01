use alloy_primitives::{address, Address, B256, U256};
use reth_errors::ProviderResult;
use reth_provider::StateProvider;

/// MEV Blocker Fee Till contract address.
pub const MEV_BLOCKER_FEE_TILL_ADDRESS: Address =
    address!("08cd77feb3fb28cc1606a91e0ea2f5e3eaba1a9a");

/// MEV Blocker Fee Till contract price slot.
pub const MEV_BLOCKER_FEE_TILL_PRICE_SLOT: B256 = B256::with_last_byte(6);

/// The upper bound for the mevblocker block price - 10^16.
pub const MEV_BLOCKER_PRICE_LIMIT: U256 = U256::from_limbs([0x2386f26fc10000, 0, 0, 0]);

/// Retrieve mevblocker block price from the contract.
/// NOTE: The block price is set to `0` if the limit is exceeded.
pub fn get_mevblocker_price<P: StateProvider>(provider: P) -> ProviderResult<U256> {
    let mut mev_blocker_price = provider
        .storage(
            MEV_BLOCKER_FEE_TILL_ADDRESS,
            MEV_BLOCKER_FEE_TILL_PRICE_SLOT,
        )?
        .unwrap_or_default();
    if mev_blocker_price > MEV_BLOCKER_PRICE_LIMIT {
        tracing::warn!(%mev_blocker_price, limit = %MEV_BLOCKER_PRICE_LIMIT, "MEV blocker price too high, setting to 0");
        mev_blocker_price = U256::ZERO;
    }
    Ok(mev_blocker_price)
}
