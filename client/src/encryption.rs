use std::rc::Rc;

use aes_gcm_siv::{
    aead::{Aead, KeyInit},
    Aes256GcmSiv, Nonce,
};
use rand::{rngs::OsRng, thread_rng, Rng};
use strum_macros::{Display, EnumString};
use ya_relay_core::crypto::{CryptoProvider, PublicKey, SessionCrypto};
use ya_relay_proto::proto::Payload;

use crate::error::EncryptionError;

#[derive(Display, EnumString, PartialEq)]
enum EncryptionType {
    Aes256GcmSiv,
}

pub trait Encryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn encryption_flag(&self) -> bool;
}

pub fn new(
    supported_encryptions: &Vec<String>,
    remote_session_key: &Option<PublicKey>,
    session_crypto: &SessionCrypto,
) -> Box<dyn Encryption> {
    // TODO: Define who new encryptions will negotiated in the future.
    if let Some(key) = remote_session_key {
        if supported_encryptions.contains(&EncryptionType::Aes256GcmSiv.to_string()) {
            let shared_secret = session_crypto.secret_with(key);
            Box::new(Aes256GcmSivEncryption::new(shared_secret))
        } else {
            log::warn!("Could not negotiate encryption type");
            Box::new(NullEncryption {})
        }
    } else {
        Box::new(NullEncryption {})
    }
}

pub fn supported_encryptions() -> Vec<String> {
    vec![EncryptionType::Aes256GcmSiv.to_string()]
}

struct NullEncryption;

impl Encryption for NullEncryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }

    fn encryption_flag(&self) -> bool {
        false
    }
}

pub struct Aes256GcmSivEncryption {
    cipher: Aes256GcmSiv,
}

impl Aes256GcmSivEncryption {
    pub fn new(shared_secret: [u8; 32]) -> Self {
        Self {
            cipher: Aes256GcmSiv::new(&shared_secret.into()),
        }
    }
}

impl Encryption for Aes256GcmSivEncryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        let nonce = thread_rng().gen::<[u8; 12]>();
        self.cipher
            .encrypt(&Nonce::from_slice(&nonce), packet.as_ref())
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|mut ciphertext| {
                ciphertext.splice(0..0, nonce.iter().cloned());
                Payload::from(ciphertext)
            })
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        let mut packet = packet.into_vec();
        let nonce = Nonce::from_slice(&packet[0..12]);
        self.cipher
            .decrypt(&nonce, &packet[12..])
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|plaintext| Payload::from(plaintext))
    }

    fn encryption_flag(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_encryption_negotiation() {
        let session_crypto = SessionCrypto::generate().unwrap();
        let remote_session_key = Some(session_crypto.pub_key());
        let supported_encryption = supported_encryptions();

        assert_eq!(new(&supported_encryption, &None, &session_crypto).encryption_flag(), false);
        assert_eq!(new(&vec!["other_cipher".to_string()], &remote_session_key, &session_crypto).encryption_flag(), false);
        assert_eq!(new(&supported_encryption, &remote_session_key, &session_crypto).encryption_flag(), true);
    }

    #[test]
    fn test_encryption() {
        let node_a_session_crypto = SessionCrypto::generate().unwrap();
        let node_a_session_key = Some(node_a_session_crypto.pub_key());
        let node_b_session_crypto = SessionCrypto::generate().unwrap();
        let node_b_session_key = Some(node_b_session_crypto.pub_key());
        let supported_encryptions = supported_encryptions();

        let node_a_encryption = new(&supported_encryptions, &node_b_session_key, &node_a_session_crypto);
        let node_b_encryption = new(&supported_encryptions, &node_a_session_key, &node_b_session_crypto);

        let plaintext = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Duis quis nisi vel sem iaculis maximus.";
        let payload = Payload::from(plaintext.to_vec());

        let ciphertext_a_to_b = node_a_encryption.encrypt(payload.clone()).unwrap();
        let recovered_payload_at_b = node_b_encryption.decrypt(ciphertext_a_to_b.clone()).unwrap();

        assert_eq!(recovered_payload_at_b.as_ref(), plaintext);

        let ciphertext_b_to_a = node_b_encryption.encrypt(payload.clone()).unwrap();
        assert_ne!(ciphertext_a_to_b.as_ref(), ciphertext_b_to_a.as_ref());

        let recovered_payload_at_a = node_a_encryption.decrypt(ciphertext_b_to_a).unwrap();

        assert_eq!(recovered_payload_at_a.as_ref(), plaintext);

        let ciphertext_a_to_b_2 = node_a_encryption.encrypt(payload.clone()).unwrap();
        assert_ne!(ciphertext_a_to_b.as_ref(), ciphertext_a_to_b_2.as_ref());
    }
}
