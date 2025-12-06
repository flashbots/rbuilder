use std::{cmp::Ordering, sync::Arc};

use alloy_primitives::{Address, U256};
use orderpool_global_priority::block_pool::BlockOrderpool;
use orderpool_global_priority::Order as OrderTrait;
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use reth_errors::ProviderError;
use tokio_util::sync::CancellationToken;

use crate::{
    live_builder::order_input::{order_sink::OrderPoolCommand, orderpool::OrdersForBlock},
    utils::NonceCache,
};

#[derive(Debug, Clone, Default, PartialOrd, PartialEq, Eq)]
pub struct OrderScore {
    pub is_simulated: bool,
    pub high_priority: bool,
    pub profit: U256,
}

impl Ord for OrderScore {
    fn cmp(&self, other: &Self) -> Ordering {
        let sim_cmp = self.is_simulated.cmp(&other.is_simulated);
        let prio_cmp = self.high_priority.cmp(&other.high_priority);

        sim_cmp
            .then(prio_cmp)
            .then_with(|| self.profit.cmp(&other.profit))
    }
}

pub type NewOrderPool = BlockOrderpool<NewOrderpoolOrder, NonceCache>;

pub async fn push_to_new_orderpool(
    mut orders_for_block: OrdersForBlock,
    pool: NewOrderPool,
    block_cancellation: CancellationToken,
) {
    loop {
        let command = tokio::select! {
            recv = orders_for_block.new_order_sub.recv() => {
            if let Some(recv) = recv {
            recv
            } else {
            return;
            }
        }
                _ = block_cancellation.cancelled() => {
                    return;
                }
        };
        match command {
            OrderPoolCommand::Insert(order) => pool.add_order(NewOrderpoolOrder::new(order)),
            OrderPoolCommand::Remove(order_id) => pool.delete_order(&order_id),
        }
    }
}

impl orderpool_global_priority::NonceSource for NonceCache {
    type NonceError = ProviderError;

    fn nonce(&self, account: &Address) -> Result<u64, Self::NonceError> {
        self.nonce(*account)
    }
}

#[derive(Debug, Clone)]
pub struct NewOrderpoolOrder {
    pub id: OrderId,
    pub order: Order,
    pub sim_order: Option<Arc<SimulatedOrder>>,
    pub score: OrderScore,
    pub nonces: Vec<orderpool_global_priority::OrderNonce>,
}

impl NewOrderpoolOrder {
    pub fn new(order: Order) -> Self {
        let id = order.id();
        // TODO: we start with these nonces, but we update them using simulation result
        let nonces = order
            .nonces()
            .into_iter()
            .map(|n| orderpool_global_priority::OrderNonce {
                nonce: orderpool_global_priority::AccountNonce {
                    address: n.address,
                    value: n.nonce,
                },
                optional: n.optional,
                increment: 1,
            })
            .collect::<Vec<_>>();
        Self {
            id,
            order,
            sim_order: None,
            score: OrderScore::default(),
            nonces,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewOrderpoolUpdate {
    pub sim_order: Arc<SimulatedOrder>,
}

impl OrderTrait for NewOrderpoolOrder {
    type Score = OrderScore;

    type Update = NewOrderpoolUpdate;

    type ID = OrderId;

    fn score(&self) -> &Self::Score {
        &self.score
    }

    fn update(&mut self, update: Self::Update) {
        self.score.profit = update
            .sim_order
            .sim_value
            .non_mempool_profit_info()
            .coinbase_profit();
        self.sim_order = Some(update.sim_order);
        self.score.is_simulated = true;
    }

    fn id(&self) -> &Self::ID {
        &self.id
    }

    fn block_range(&self) -> (u64, u64) {
        // we only use block orderpool here
        (0, 0)
    }

    fn nonces(&self) -> &[orderpool_global_priority::OrderNonce] {
        &self.nonces
    }
}
