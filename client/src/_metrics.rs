use crate::_session_layer::ConnectionMethod;
use metrics::{
    describe_counter, describe_gauge, gauge, increment_counter, register_counter, register_gauge,
    Unit,
};
use ya_relay_core::NodeId;

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
    register_counter!("ya-relay.client.session.established");
    register_counter!("ya-relay.client.session.closed");
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
    describe_counter!(
        "ya-relay.client.session.closed",
        Unit::Count,
        "Incremented when session (either p2p or relayed) is closed.\
        Metric can be used to track stability of connection."
    );
    describe_counter!(
        "ya-relay.client.session.established",
        Unit::Count,
        "Incremented when session (either p2p or relayed) is established.\
        Metric can be used to track stability of connection."
    );
}

pub fn metric_session_established(node_id: NodeId, method: ConnectionMethod) {
    gauge!("ya-relay.client.session.type", method.metric(), TARGET_ID => node_id.to_string());
    increment_counter!("ya-relay.client.session.established", TARGET_ID => node_id.to_string());
}
