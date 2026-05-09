use parking_lot::RwLock;
use rbuilder_primitives::epbs::ProposerPreferences;
use std::collections::HashMap;
use tracing::debug;

/// Cache of proposer preferences keyed by proposal slot.
#[derive(Debug)]
pub struct ProposerPreferencesCache {
    /// slot -> ProposerPreferences mapping.
    prefs: RwLock<HashMap<u64, ProposerPreferences>>,
}

impl Default for ProposerPreferencesCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ProposerPreferencesCache {
    pub fn new() -> Self {
        Self {
            prefs: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or update preferences for a slot.
    pub fn insert(&self, prefs: ProposerPreferences) {
        let slot = prefs.proposal_slot;
        debug!(
            slot,
            validator_index = prefs.validator_index,
            fee_recipient = %prefs.fee_recipient,
            gas_limit = prefs.gas_limit,
            "Cached proposer preferences"
        );
        self.prefs.write().insert(slot, prefs);
    }

    pub fn get(&self, slot: u64) -> Option<ProposerPreferences> {
        self.prefs.read().get(&slot).cloned()
    }

    /// cleanup up preferences older than current_slot - max_age_slots.
    pub fn cleanup(&self, current_slot: u64, max_age_slots: u64) {
        let cutoff = current_slot.saturating_sub(max_age_slots);
        self.prefs.write().retain(|&slot, _| slot >= cutoff);
    }

    pub fn len(&self) -> usize {
        self.prefs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.prefs.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn make_prefs(slot: u64) -> ProposerPreferences {
        ProposerPreferences {
            proposal_slot: slot,
            validator_index: 42,
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
        }
    }

    #[test]
    fn test_insert_and_get() {
        let cache = ProposerPreferencesCache::new();
        cache.insert(make_prefs(100));
        assert!(cache.get(100).is_some());
        assert!(cache.get(101).is_none());
    }

    #[test]
    fn test_cleanup() {
        let cache = ProposerPreferencesCache::new();
        cache.insert(make_prefs(10));
        cache.insert(make_prefs(20));
        cache.insert(make_prefs(30));
        cache.cleanup(30, 15);
        // slot 10 is < 30 - 15 = 15, should be removed
        assert!(cache.get(10).is_none());
        assert!(cache.get(20).is_some());
        assert!(cache.get(30).is_some());
    }
}
