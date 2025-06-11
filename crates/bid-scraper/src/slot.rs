#![allow(dead_code)]

const MAINNET_GENESIS_SLOT_TIMESTAMP: u64 = 1606824023;
const MAINNET_SLOT_DURATION: u64 = 12;

pub fn get_epoch_number(slot_number: u64) -> u64 {
    // Returns what epoch a slot number corresponds to
    slot_number / 32
}

pub fn get_seconds_in_slot(timestamp: f64) -> f64 {
    /* Return a float between 0 and MAINNET_SLOT_DURATION

    This tells us how many seconds into the slot `timestamp` is

    This is most useful to know when we are in the slot, and decide at what point we want to start
    sending transactions.
    Example: we want to send bundles late to reduce adverse selection.
    */
    (timestamp - MAINNET_GENESIS_SLOT_TIMESTAMP as f64) % (MAINNET_SLOT_DURATION as f64)
}

pub fn get_seconds_in_specific_slot(timestamp: f64, slot_number: u64) -> f64 {
    /*Returns a float corresponding to how many seconds into
    `slot_number` we are

    Sometimes we might want to know how many seconds we are in a given slot
    since we might want to keep sending until a new slot is mined.
    Even if we go above MAINNET_SLOT_DURATION seconds
    */
    timestamp - (MAINNET_GENESIS_SLOT_TIMESTAMP as f64 + 12. * slot_number as f64)
}

pub fn get_slot_number(timestamp: u64) -> u64 {
    // Return slot number corresponding to given timestamp
    (timestamp - MAINNET_GENESIS_SLOT_TIMESTAMP) / MAINNET_SLOT_DURATION
}

pub fn get_slot_timestamp(slot_number: u64) -> u64 {
    // Returns the utc timestamp of `slot_number`
    MAINNET_GENESIS_SLOT_TIMESTAMP + slot_number * MAINNET_GENESIS_SLOT_TIMESTAMP
}
