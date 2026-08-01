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
            Self::Aes256GcmSiv => "Aes256GcmSiv",
        }
    }
}

pub trait Encryption {
    fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError>;
    fn encryption_flag(&self) -> bool;
}

#[cfg(feature = "encryption")]
pub fn new(
    supported_encryption: Vec<String>,
    remote_session_key: Option<PublicKey>,
    session_crypto: SessionCrypto,
) -> Box<dyn Encryption> {
    //
    if let Some(key) = remote_session_key {
        if supported_encryption
            .iter()
            .any(|enc| enc == EncryptionType::Aes256GcmSiv.as_str())
        {
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

#[cfg(not(feature = "encryption"))]
pub fn new(
    _supported_encryption: Vec<String>,
    _remote_session_key: Option<PublicKey>,
    _session_crypto: SessionCrypto,
) -> Box<dyn Encryption> {
    Box::new(NullEncryption {})
}

#[cfg(feature = "encryption")]
pub fn supported_encryptions() -> Vec<String> {
    vec![EncryptionType::Aes256GcmSiv.to_string()]
}

#[cfg(not(feature = "encryption"))]
pub fn supported_encryptions() -> Vec<String> {
    vec![]
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
            .encrypt(Nonce::from_slice(&nonce), packet.as_ref())
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|mut ciphertext| {
                ciphertext.splice(0..0, nonce.iter().cloned());
                Payload::from(ciphertext)
            })
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        const NONCE_SIZE: usize = 12;
        const TAG_SIZE: usize = 16;

        let packet = packet.into_vec();
        if packet.len() < NONCE_SIZE + TAG_SIZE {
            return Err(EncryptionError::Generic(format!(
                "Encrypted payload is too short: expected at least {} bytes, got {}",
                NONCE_SIZE + TAG_SIZE,
                packet.len()
            )));
        }
        let nonce = Nonce::from_slice(&packet[0..12]);
        self.cipher
            .decrypt(nonce, &packet[12..])
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(Payload::from)
    }

    fn encryption_flag(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{new, Aes256GcmSivEncryption, Encryption};
    use ya_relay_core::crypto::SessionCrypto;
    use ya_relay_proto::proto::Payload;

    #[cfg(not(feature = "encryption"))]
    #[test]
    fn does_not_negotiate_encryption_when_feature_is_disabled() {
        let local = SessionCrypto::generate().unwrap();
        let remote = SessionCrypto::generate().unwrap();

        let encryption = new(
            vec!["Aes256GcmSiv".to_string()],
            Some(remote.pub_key()),
            local,
        );

        assert!(!encryption.encryption_flag());
    }

    #[cfg(feature = "encryption")]
    #[test]
    fn negotiates_supported_encryption_when_feature_is_enabled() {
        let local = SessionCrypto::generate().unwrap();
        let remote = SessionCrypto::generate().unwrap();

        let encryption = new(
            vec!["Aes256GcmSiv".to_string()],
            Some(remote.pub_key()),
            local,
        );

        assert!(encryption.encryption_flag());
    }

    #[test]
    fn rejects_truncated_encrypted_payloads() {
        let encryption = Aes256GcmSivEncryption::new([42; 32]);

        for len in [0, 11, 12, 27] {
            let result = encryption.decrypt(Payload::from(vec![0; len]));
            assert!(result.is_err(), "payload of length {} was accepted", len);
        }
    }

    #[test]
    fn encrypts_and_decrypts_payload() {
        let encryption = Aes256GcmSivEncryption::new([42; 32]);
        let plaintext = Payload::from(b"hello".to_vec());

        let encrypted = encryption.encrypt(plaintext.clone()).unwrap();
        let decrypted = encryption.decrypt(encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }
}
