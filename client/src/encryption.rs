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

#[derive(Display, PartialEq)]
enum EncryptionType {
    Aes256GcmSiv,
}

impl EncryptionType {

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aes256GcmSiv => "Aes256GcmSiv"
        }
    }

}

pub trait Encryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn encryption_flag(&self) -> bool;
}

pub fn new(
    supported_encryption: Vec<String>,
    remote_session_key: Option<PublicKey>,
    session_crypto: SessionCrypto,
) -> Box<dyn Encryption> {
    //
    if let Some(key) = remote_session_key {
        if supported_encryption.iter().any(|enc| enc == EncryptionType::Aes256GcmSiv.as_str()) {
            let shared_secret = session_crypto.secret_with(&key);
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
