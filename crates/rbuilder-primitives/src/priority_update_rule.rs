use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, FixedBytes};
use serde::Deserialize;
use std::collections::HashSet;

use crate::TransactionSignedEcRecoveredWithBlobs;

/// 4-byte function selector.
pub type Selector = FixedBytes<4>;

/// Rule used to classify an order as a priority update.
///
/// An order matches when *every* transaction in the order satisfies all of:
/// - `allowed_from` is empty OR contains `tx.from`
/// - `allowed_to` is empty OR contains `tx.to`
/// - `allowed_signatures` is empty OR contains the first 4 bytes of `tx.input`
///
/// `force_top_of_block` controls downstream inclusion semantics:
/// - `true`: matched orders are routed to a separate top-of-block bucket and
///   are always committed at the top of the block, regardless of demand.
/// - `false`: matched orders are routed to the regular priority-update pool
///   and are only committed when *used* by another order. An order is
///   considered to use a priority update when, during its simulation, it
///   reads a storage slot that the priority update writes — i.e. the regular
///   order observes the post-update value via the PU overlay. At commit
///   time, the touched priority updates are committed before the order.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct PriorityUpdateRule {
    #[serde(default)]
    pub allowed_from: HashSet<Address>,
    #[serde(default)]
    pub allowed_to: HashSet<Address>,
    #[serde(default)]
    pub allowed_signatures: HashSet<Selector>,
    #[serde(default)]
    pub force_top_of_block: bool,
}

/// Result of classifying an order against a set of [`PriorityUpdateRule`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorityUpdateClass {
    /// Matched a rule with `force_top_of_block = true`.
    ForceTopOfBlock,
    /// Matched a regular priority-update rule.
    Regular,
}

impl PriorityUpdateRule {
    /// Returns true when every transaction in the (non-empty) `txs` slice
    /// satisfies this rule.
    pub fn matches(&self, txs: &[TransactionSignedEcRecoveredWithBlobs]) -> bool {
        if txs.is_empty() {
            return false;
        }

        txs.iter().all(|tx| {
            if !self.allowed_from.is_empty() && !self.allowed_from.contains(&tx.signer()) {
                return false;
            }

            if !self.allowed_to.is_empty() {
                match tx.to() {
                    Some(to) if self.allowed_to.contains(&to) => {}
                    _ => return false,
                }
            }

            if !self.allowed_signatures.is_empty() {
                let input = tx.internal_tx_unsecure().input();
                if input.len() < 4 {
                    return false;
                }
                let selector = Selector::from_slice(&input[..4]);
                if !self.allowed_signatures.contains(&selector) {
                    return false;
                }
            }

            true
        })
    }

    /// Classify the txs against `rules`. Returns the strongest classification:
    /// `ForceTopOfBlock` wins over `Regular`.
    pub fn match_rules(
        txs: &[TransactionSignedEcRecoveredWithBlobs],
        rules: &[Self],
    ) -> Option<PriorityUpdateClass> {
        let mut hit = None;
        for rule in rules {
            if !rule.matches(txs) {
                continue;
            }
            if rule.force_top_of_block {
                return Some(PriorityUpdateClass::ForceTopOfBlock);
            }
            hit = Some(PriorityUpdateClass::Regular);
        }
        hit
    }
}
