use std::{cmp::Ordering, sync::Arc};

use alloy_primitives::{Address, U256};
use orderpool_global_priority::{cross_block_pool::Orderpool, Order as OrderTrait};
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use reth_errors::ProviderError;

use crate::utils::NonceCache;


#[derive(Debug, Clone, Default, PartialOrd, PartialEq, Eq)]
pub struct OrderScore {
    pub high_priority: bool, 
    pub non_mempool_profit: U256,
}

impl Ord for OrderScore {
    fn cmp(&self, other: &Self) -> Ordering {
	match (self.high_priority, other.high_priority) {
	    (true, false) => Ordering::Greater,
	    (false, true) => Ordering::Less,
	    _ => self.non_mempool_profit.cmp(&other.non_mempool_profit),
	}
    }
}

pub type NewOrderPool = Orderpool<NewOrderpoolOrder, NonceCache>;


impl orderpool_global_priority::NonceSource for NonceCache {
    type NonceError = ProviderError;

    fn nonce(&self, account: &Address) -> Result<u64, Self::NonceError> {
	self.nonce(*account)
    }
}


#[derive(Debug, Clone)]
pub struct NewOrderpoolOrder {
    pub id: OrderId,
    pub block_range: (u64, u64),
    pub order: Order,
    pub sim_order: Option<Arc<SimulatedOrder>>,
    pub score: OrderScore,
    pub nonces: Vec<orderpool_global_priority::OrderNonce>,
}

impl NewOrderpoolOrder {
    pub fn new(order: Order, last_onchain_block: u64) -> Self {
	let id = order.id();
	let block_range  = if let Some(mut target_block) = order.target_block() {
	    if target_block == 0 {
		target_block = last_onchain_block + 1;
	    }
	    (target_block, target_block) 
	} else {
	    match &id {
		OrderId::Tx(_) => (last_onchain_block + 1, last_onchain_block + 10), // TODO: use proper value
		_ => (last_onchain_block + 1, last_onchain_block + 1),
	    }
	};
	// TODO: we start with these nonces, but we update them using simulation result
	let nonces = order.nonces().into_iter().map(|n| {
	    orderpool_global_priority::OrderNonce {
		nonce: orderpool_global_priority::AccountNonce {
		    address: n.address,
		    value: n.nonce,
		},
		optional: n.optional,
		increment: 1,
	    }
	}).collect::<Vec<_>>();
	Self {
	    id,
	    block_range,
	    order,
	    sim_order: None,
	    score: OrderScore::default(),
	    nonces,
	}
    }
}

#[derive(Debug, Clone)]
pub struct Orderpool2OrderUpdate {
    sim_order: Arc<SimulatedOrder>,
}

impl OrderTrait for NewOrderpoolOrder {
    type Score = OrderScore;

    type Update = Orderpool2OrderUpdate;

    type ID = OrderId;

    fn score(&self) -> &Self::Score {
	&self.score
    }

    fn update(&mut self, update: Self::Update) {
	self.sim_order = Some(update.sim_order);
    }

    fn id(&self) -> &Self::ID {
	&self.id
    }

    fn block_range(&self) -> (u64, u64) {
	self.block_range
    }

    fn nonces(&self) -> &[orderpool_global_priority::OrderNonce] {
	&self.nonces
    }
}
