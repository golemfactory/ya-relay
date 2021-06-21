#[cfg(feature = "codec")]
pub mod codec;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/ya_relay_proto.rs"));
}
