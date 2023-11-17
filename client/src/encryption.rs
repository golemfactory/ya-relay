use std::rc::Rc;

use libaes::Cipher;
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
        Box::new(Aes128CbcEncryption::new(key, session_crypto))
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

pub struct Aes128CbcEncryption {
    cipher: Cipher,
    iv: [u8; 16],
}

impl Aes128CbcEncryption {
    pub fn new(
        remote_session_key: PublicKey,
        session_crypto: SessionCrypto,
    ) -> Self {
        let shared_secret = session_crypto.secret_with(&remote_session_key);
        let mut key: [u8; 16] = [0; 16];
        key.copy_from_slice(&shared_secret[..16]);
        let mut iv: [u8; 16] = [0; 16];
        iv.copy_from_slice(&shared_secret[16..]);
        Self {
            cipher: Cipher::new_128(&key),
            iv,
        }
    }
}

impl Encryption for Aes128CbcEncryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(Payload::from(self.cipher.cbc_encrypt(&self.iv, packet.as_ref())))
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(Payload::from(self.cipher.cbc_decrypt(&self.iv, packet.as_ref())))
    }

    fn encryption_flag(&self) -> bool {
        true
    }
}
