use alloy_primitives::hex;
use ethereum_consensus::crypto::SecretKey;
use rand;

pub fn generate_random_bls_address() -> String {
    let mut rng = rand::thread_rng();
    let sk = SecretKey::random(&mut rng).unwrap();
    let pk = sk.public_key();
    let raw_bytes = pk.as_ref();
    hex::encode(raw_bytes)
}

#[cfg(test)]
mod tests {
    use crate::utils::bls::generate_random_bls_address;

    #[test]
    fn test_generate_random_bls_address() {
        let bls_address = generate_random_bls_address();
        assert_eq!(bls_address.len(), 96, "BLS address should be of 96 length");
    }
}
