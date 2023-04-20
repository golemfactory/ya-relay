use std::collections::HashMap;

use ya_relay_core::NodeId;

use crate::_routing_session::Routing;
use crate::_session_layer::SessionLayer;
use crate::_virtual_layer::TcpLayer;
use crate::client::ForwardSender;

/// Responsible for sending data. Handles different kinds of transport types.
struct TransportLayer {
    session_layer: SessionLayer,
    virtual_tcp: TcpLayer,

    forward_unreliable: HashMap<NodeId, Routing>,
    forward_transfer: HashMap<NodeId, ForwardSender>,
    forward_reliable: HashMap<NodeId, ForwardSender>,
}

impl TransportLayer {}
