pub mod challenge;
pub mod crypto;
pub mod error;
pub mod key;
pub mod session;
pub mod udp_stream;
pub mod utils;

pub use ya_client_model::NodeId;

use std::time::Duration;
use utils::typed_from_env;

lazy_static::lazy_static! {
    pub static ref DEFAULT_NET_PORT: u16 = typed_from_env::<u16>("DEFAULT_NET_PORT", 7464);
    pub static ref FORWARDER_RATE_LIMIT: u32 = typed_from_env::<u32>("FORWARDER_RATE_LIMIT", 2048);
    pub static ref FORWARDER_RESUME_INTERVAL: Duration = Duration::new(typed_from_env::<u64>("FORWARDER_RESUME_INTERVAL",1), 0); // seconds
}
