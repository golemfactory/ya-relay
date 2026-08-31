use std::mem::size_of;
use std::rc::Rc;

use aes_gcm_siv::{
    aead::{Aead, AeadCore, KeyInit},
    Aes256GcmSiv, Nonce, Tag,
};
use rand::thread_rng;
use strum_macros::{Display, EnumString};
use ya_relay_core::crypto::{CryptoProvider, PublicKey, SessionCrypto};
use ya_relay_proto::proto::Payload;

use crate::error::EncryptionError;

const AES_256_GCM_SIV_CIPHERTEXT_EXPANSION: usize = size_of::<Nonce>() + size_of::<Tag>();

#[cfg(feature = "encryption")]
pub(crate) const MAX_CIPHERTEXT_EXPANSION: usize = AES_256_GCM_SIV_CIPHERTEXT_EXPANSION;
#[cfg(not(feature = "encryption"))]
pub(crate) const MAX_CIPHERTEXT_EXPANSION: usize = 0;

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
) -> Result<Box<dyn Encryption>, EncryptionError> {
    let encryption: Box<dyn Encryption> = if let Some(key) = remote_session_key {
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
    };

    enforce_encryption_policy(encryption)
}

#[cfg(not(feature = "encryption"))]
pub fn new(
    _supported_encryption: Vec<String>,
    _remote_session_key: Option<PublicKey>,
    _session_crypto: SessionCrypto,
) -> Result<Box<dyn Encryption>, EncryptionError> {
    enforce_encryption_policy(Box::new(NullEncryption {}))
}

fn enforce_encryption_policy(
    encryption: Box<dyn Encryption>,
) -> Result<Box<dyn Encryption>, EncryptionError> {
    #[cfg(feature = "encryption-strict")]
    if !encryption.encryption_flag() {
        return Err(EncryptionError::Generic(
            "Peer does not support required session encryption".to_string(),
        ));
    }

    Ok(encryption)
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
        let nonce = Aes256GcmSiv::generate_nonce(&mut thread_rng());
        self.cipher
            .encrypt(&nonce, packet.as_ref())
            .map_err(|e| EncryptionError::Generic(e.to_string()))
            .map(|mut ciphertext| {
                ciphertext.splice(0..0, nonce.iter().cloned());
                Payload::from(ciphertext)
            })
    }

    fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        let packet = packet.into_vec();
        if packet.len() < AES_256_GCM_SIV_CIPHERTEXT_EXPANSION {
            return Err(EncryptionError::Generic(format!(
                "Encrypted payload is too short: expected at least {} bytes, got {}",
                AES_256_GCM_SIV_CIPHERTEXT_EXPANSION,
                packet.len()
            )));
        }
        let (nonce, ciphertext) = packet.split_at(size_of::<Nonce>());
        let nonce = Nonce::from_slice(nonce);
        self.cipher
            .decrypt(nonce, ciphertext)
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
        )
        .unwrap();

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
        )
        .unwrap();

        assert!(encryption.encryption_flag());
    }

    #[cfg(all(feature = "encryption", not(feature = "encryption-strict")))]
    #[test]
    fn falls_back_to_plaintext_when_peer_does_not_support_encryption() {
        let local = SessionCrypto::generate().unwrap();

        let encryption = new(vec![], None, local).unwrap();

        assert!(!encryption.encryption_flag());
    }

    #[cfg(feature = "encryption-strict")]
    #[test]
    fn rejects_peer_without_session_encryption() {
        let local = SessionCrypto::generate().unwrap();

        assert!(new(vec![], None, local).is_err());
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
        assert_eq!(
            encrypted.len(),
            plaintext.len() + super::AES_256_GCM_SIV_CIPHERTEXT_EXPANSION
        );
        let decrypted = encryption.decrypt(encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }
}
