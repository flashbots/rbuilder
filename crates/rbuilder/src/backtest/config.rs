use revm_primitives::hex;
use secp256k1::rand::rngs::OsRng;
use secp256k1::SecretKey; 

use std::env;

// Generate a random private key 
fn generate_private_key() -> String {
    let mut rng = OsRng;
    let secret_key = SecretKey::new(&mut rng); 
    let hex_secret_key =  hex::encode(secret_key.secret_bytes());
    hex_secret_key
}

// Add COINBASE_SECRET_KEY as an environment variable during runtime
pub fn add_env_coinbase_signer() {
    let private_key = generate_private_key();
    env::set_var("COINBASE_SECRET_KEY", private_key);
} 
