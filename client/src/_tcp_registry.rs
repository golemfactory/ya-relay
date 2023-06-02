use derive_more::Display;

use ya_relay_core::NodeId;
use ya_relay_stack::ya_smoltcp::wire::IpEndpoint;
use ya_relay_stack::Connection;

use crate::_routing_session::RoutingSender;

// TODO: Try to unify with `TransportType`.
// TODO: We could support any number of channels. User of the library could decide.
#[derive(Clone, Copy, Display)]
pub enum ChannelType {
    Messages = 1,
    Transfer = 2,
}

/// Information about virtual node in TCP network built over UDP protocol.
#[derive(Clone)]
pub struct VirtNode {
    pub id: NodeId,
    pub endpoint: IpEndpoint,
    pub session: RoutingSender,
}

#[derive(Clone)]
pub struct VirtConnection {
    pub id: NodeId,
    pub conn: Connection,
}
