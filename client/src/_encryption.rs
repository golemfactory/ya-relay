use std::rc::Rc;
use std::sync::Arc;

use ya_relay_core::crypto::CryptoProvider;

/// Encrypting packets, solving challenges, proving identity.
pub struct Encryption {
    pub crypto: Rc<dyn CryptoProvider>,
}
