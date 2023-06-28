use metrics::{describe_counter, describe_gauge, register_counter, register_gauge, Unit};

pub static SOURCE_ID: &str = "SourceId";
pub static TARGET_ID: &str = "TargetId";
pub static RELAY_ID: &str = "RelayId";

pub fn register_metrics() {
    register_counter!("ya-relay.packet.tcp.outgoing.size");
    register_counter!("ya-relay.packet.tcp.outgoing.num");
    register_counter!("ya-relay.packet.tcp.incoming.size");
    register_counter!("ya-relay.packet.tcp.incoming.num");
    register_counter!("ya-relay.packet.udp.outgoing.size");
    register_counter!("ya-relay.packet.udp.outgoing.num");
    register_counter!("ya-relay.packet.udp.incoming.size");
    register_counter!("ya-relay.packet.udp.incoming.num");
    register_gauge!("ya-relay.client.session.type");
    register_gauge!("ya-relay.client.public-address");

    describe_counter!(
        "ya-relay.packet.tcp.outgoing.size",
        Unit::Bytes,
        "Size of outgoing tcp packets (including tcp headers and Forward packet size)"
    );
    describe_counter!(
        "ya-relay.packet.tcp.outgoing.size",
        Unit::Count,
        "Number of outgoing tcp packets"
    );
    describe_gauge!(
        "ya-relay.client.session.type",
        "Type of established session with Node. Check `ConnectionMethod` for numbers meaning."
    );
}
