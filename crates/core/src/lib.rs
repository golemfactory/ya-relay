pub mod challenge;
pub mod crypto;
pub mod error;
pub mod key;
pub mod session;
pub mod udp_stream;
pub mod utils;

use std::time::Duration;
use utils::typed_from_env;

lazy_static::lazy_static! {
    pub static ref DEFAULT_NET_PORT: u16 = typed_from_env::<u16>("DEFAULT_NET_PORT", 7464);
    pub static ref SESSION_CLEANER_INTERVAL: Duration = Duration::new(typed_from_env::<u64>("SESSION_CLEANER_INTERVAL", 10), 0); // seconds
    pub static ref SESSION_TIMEOUT: i64 = typed_from_env::<i64>("SESSION_TIMEOUT", 60); // seconds
}
