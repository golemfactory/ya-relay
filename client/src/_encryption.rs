use std::rc::Rc;

use ya_relay_core::crypto::CryptoProvider;

/// Encrypting packets, solving challenges, proving identity.
#[derive(Clone)]
pub struct Encryption {
    pub crypto: Rc<dyn CryptoProvider>,
}