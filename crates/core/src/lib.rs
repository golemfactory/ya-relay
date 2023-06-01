pub mod challenge;
pub mod crypto;
pub mod dispatch;
pub mod error;
pub mod identity;
pub mod key;
pub mod server_session;
pub mod session;
pub mod sync;
pub mod udp_stream;
pub mod utils;

pub use ya_client_model::NodeId;

use utils::typed_from_env;

lazy_static::lazy_static! {
    pub static ref DEFAULT_NET_PORT: u16 = typed_from_env::<u16>("DEFAULT_NET_PORT", 7464);
}
