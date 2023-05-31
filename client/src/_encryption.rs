use std::rc::Rc;

use ya_relay_core::crypto::CryptoProvider;
use ya_relay_proto::proto::Payload;

use crate::_error::EncryptionError;

/// Encrypting packets, solving challenges, proving identity.
#[derive(Clone)]
pub struct Encryption {
    pub crypto: Rc<dyn CryptoProvider>,
}

impl Encryption {
    pub async fn encrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }

    pub async fn decrypt(&self, packet: Payload) -> Result<Payload, EncryptionError> {
        Ok(packet)
    }
}
