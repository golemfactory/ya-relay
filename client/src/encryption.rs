use std::rc::Rc;

use aes_gcm_siv::{
    aead::{Aead, KeyInit},
    Aes128GcmSiv,
};
use rand::{Rng, rngs::OsRng};
use ya_relay_core::crypto::{CryptoProvider, PublicKey, SessionCrypto};
use ya_relay_proto::proto::Payload;

use crate::error::EncryptionError;

pub trait Encryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn encryption_flag(&self) -> bool;
}

pub fn new(
    _crypto: Rc<dyn CryptoProvider>,
    remote_session_key: Option<PublicKey>,
    session_crypto: SessionCrypto,
) -> Box<dyn Encryption> {
    if let Some(key) = remote_session_key {
        let shared_secret = session_crypto.secret_with(&key);
        Box::new(Aes128GcmSivEncryption::new(shared_secret))
    } else {
        Box::new(NullEncryption {})
    }
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

pub struct Aes128GcmSivEncryption {
    cipher: Aes128GcmSiv,
}

impl Aes128GcmSivEncryption {
    pub fn new(shared_secret: [u8; 32]) -> Self {
        let mut key = [0; 16];
        key.copy_from_slice(&shared_secret[..16]);
        Aes128GcmSiv::new(&key.into());
        Self {
            cipher: Aes128GcmSiv::new(&key.into()),
        }
    }
}

impl Encryption for Aes128GcmSivEncryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        let nonce = OsRng.gen::<[u8; 12]>();
        self.cipher.encrypt(&nonce.into(), packet.as_ref())
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|mut ciphertext| {
                ciphertext.splice(0..0, nonce.iter().cloned());
                Payload::from(ciphertext)
            })
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        let mut packet = packet.into_vec();
        let nonce = packet.drain(0..12).collect::<Vec<_>>();
        self.cipher.decrypt(nonce.as_slice().into(), packet.as_slice())
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|plaintext| {
                Payload::from(plaintext)
            })
    }

    fn encryption_flag(&self) -> bool {
        true
    }
}
