use alloy_primitives::{Address, B256, U256};
use reth_primitives::{
    public_key_to_address, Signature, Transaction, TransactionSigned, TransactionSignedEcRecovered,
};
use secp256k1::{Message, SecretKey, SECP256K1};

/// Simple struct to sign txs/messages.
/// Mainly used to sign payout txs from the builder and to create test data.
#[derive(Debug, Clone)]
pub struct Signer {
    pub address: Address,
    pub secret: SecretKey,
}

impl Signer {
    pub fn try_from_secret(secret: B256) -> Result<Self, secp256k1::Error> {
        let secret = SecretKey::from_slice(secret.as_ref())?;
        let pubkey = secret.public_key(SECP256K1);
        let address = public_key_to_address(pubkey);

        Ok(Self { address, secret })
    }

    pub fn sign_message(&self, message: B256) -> Result<Signature, secp256k1::Error> {
        let s = SECP256K1
            .sign_ecdsa_recoverable(&Message::from_digest_slice(&message[..])?, &self.secret);
        let (rec_id, data) = s.serialize_compact();

        let signature = Signature {
            r: U256::try_from_be_slice(&data[..32]).expect("The slice has at most 32 bytes"),
            s: U256::try_from_be_slice(&data[32..64]).expect("The slice has at most 32 bytes"),
            odd_y_parity: rec_id.to_i32() != 0,
        };
        Ok(signature)
    }

    pub fn sign_tx(
        &self,
        tx: Transaction,
    ) -> Result<TransactionSignedEcRecovered, secp256k1::Error> {
        let signature = self.sign_message(tx.signature_hash())?;
        let signed = TransactionSigned::from_transaction_and_signature(tx, signature);
        Ok(TransactionSignedEcRecovered::from_signed_transaction(
            signed,
            self.address,
        ))
    }

    pub fn random() -> Self {
        Self::try_from_secret(B256::random()).expect("failed to create random signer")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::{address, fixed_bytes};
    use reth_primitives::{TxEip1559, TxKind as TransactionKind};

    #[test]
    fn test_sign_transaction() {
        let secret =
            fixed_bytes!("7a3233fcd52c19f9ffce062fd620a8888930b086fba48cfea8fc14aac98a4dce");
        let address = address!("B2B9609c200CA9b7708c2a130b911dabf8B49B20");
        let signer = Signer::try_from_secret(secret).expect("signer creation");
        assert_eq!(signer.address, address);

        let tx = Transaction::Eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 2,
            gas_limit: 21000,
            max_fee_per_gas: 1000,
            max_priority_fee_per_gas: 20000,
            to: TransactionKind::Call(address),
            value: U256::from(3000u128),
            ..Default::default()
        });

        let signed_tx = signer.sign_tx(tx).expect("sign tx");
        assert_eq!(signed_tx.signer(), address);

        let signed = signed_tx.into_signed();
        assert_eq!(signed.recover_signer(), Some(address));
    }
}
