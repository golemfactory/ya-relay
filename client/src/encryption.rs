use std::rc::Rc;

use ya_relay_core::crypto::{CryptoProvider, PublicKey, SessionCrypto};
use ya_relay_proto::proto::Payload;

use crate::error::EncryptionError;

/// Encrypting packets, solving challenges, proving identity.
#[derive(Clone)]
pub struct Encryption {
    crypto: Rc<dyn CryptoProvider>,
    remote_session_key: Option<PublicKey>,
    session_crypto: SessionCrypto,
}

impl Encryption {
    pub fn new(
        crypto: Rc<dyn CryptoProvider>,
        remote_session_key: Option<PublicKey>,
        session_crypto: SessionCrypto,
    ) -> Self {
        Self {
            crypto,
            remote_session_key,
            session_crypto,
        }
    }

    pub async fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }

    pub async fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }
}
