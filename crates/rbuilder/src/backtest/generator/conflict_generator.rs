pub fn distribute_transactions(
    num_slots: usize,
    total_transactions: usize,
    gradient: f64,
) -> Vec<usize> {
    //     10 / 0 --> EVENLY DISTRIBUTED
    //     10 / 2 --> MEDIUM DISTRIBUTED
    //     10 / 10 --> CONCENTRATED DISTRIBUTION
    // TO-DO: fix overflows
    // TO-DO: have at least one transaction per slot
    let mut transactions_per_slot = vec![0; num_slots];
    let mut remaining_transactions = total_transactions;

    let mut weights = vec![0.0; num_slots];
    let mut total_weight = 0.0;

    for i in 0..num_slots {
        let weight = (num_slots - i) as f64;
        weights[i] = weight.powf(gradient.min(20.0)).min(1e20);
        total_weight += weights[i];
    }

    for i in 0..num_slots {
        let proportion = weights[i] / total_weight;
        let transactions = (proportion * total_transactions as f64).round() as usize;
        transactions_per_slot[i] = transactions;
        remaining_transactions -= transactions;
    }

    // Distribute any remaining transactions evenly across slots
    let extra_transactions = remaining_transactions / num_slots;
    let mut remaining_extra = remaining_transactions % num_slots;
    for i in 0..num_slots {
        transactions_per_slot[i] += extra_transactions;
        if remaining_extra > 0 {
            transactions_per_slot[i] += 1;
            remaining_extra -= 1;
        }
    }

    transactions_per_slot
}
