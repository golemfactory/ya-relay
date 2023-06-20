use anyhow::anyhow;
use derive_more::Display;
use educe::Educe;
use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};

use ya_relay_core::NodeId;
use ya_relay_proto::proto::Payload;
use ya_relay_stack::ya_smoltcp::wire::{IpAddress, IpEndpoint};
use ya_relay_stack::Connection;

use crate::_error::{ResultExt, TcpError, TcpTransitionError};
use crate::_routing_session::RoutingSender;
use crate::_session_layer::SessionLayer;
use crate::_virtual_layer::TcpLayer;

// TODO: Try to unify with `TransportType`.
// TODO: We could support any number of channels. User of the library could decide.
#[derive(Clone, Copy, Display, Debug, PartialEq, Hash, Eq)]
pub enum ChannelType {
    Messages = 1,
    Transfer = 2,
}

#[derive(Clone, Copy, Display, Debug, PartialEq, Hash, Eq)]
pub enum ChannelDirection {
    Out,
    In,
}

/// For communication with single Node we have max 4 channels:
/// Separate channels for Messages and Transfer and each of them establishes
/// separate connection in each direction.
///
/// Having one channel for both directions would be difficult, because you need to synchronize in cases
/// when both parties are initializing connection at the same time. So maybe this choice requires more
/// resources, but is less error prone and simpler in implementation.
#[derive(Clone, Copy, Display, Debug, PartialEq, Hash, Eq)]
#[display(fmt = "{}-{}", "_0", "_1")]
pub struct ChannelDesc(pub ChannelType, pub ChannelDirection);

/// Information about virtual node in TCP network built over UDP protocol.
#[derive(Clone)]
pub struct VirtNode {
    pub address: IpAddress,
    pub routing: RoutingSender,

    /// We could support arbitrary number of channels, but this would require
    /// having dynamic data structure with locks, what is not worth at this moment.
    pub channels: [VirtChannel; 4],
}

#[derive(Clone)]
pub struct VirtChannel {
    pub channel: ChannelDesc,
    pub state: Arc<RwLock<TcpState>>,
    pub state_notifier: Arc<broadcast::Sender<Result<Arc<TcpConnection>, TcpError>>>,
}

impl VirtChannel {
    pub fn new(desc: ChannelDesc) -> VirtChannel {
        VirtChannel {
            channel: desc,
            state: Arc::new(RwLock::new(TcpState::Closed)),
            state_notifier: Arc::new(broadcast::channel(1).0),
        }
    }

    pub async fn transition(&self, new_state: TcpState) -> Result<(), TcpTransitionError> {
        let mut state = self.state.write().await;
        state.transition(new_state)
    }

    pub async fn state(&self) -> TcpState {
        self.state.read().await.clone()
    }
}

impl VirtNode {
    pub fn new(id: NodeId, layer: SessionLayer) -> VirtNode {
        let ip = IpAddress::from(to_ipv6(id.into_array()));
        let routing = RoutingSender::empty(id, layer);

        let msg_in_channel =
            VirtChannel::new(ChannelDesc(ChannelType::Messages, ChannelDirection::In));
        let msg_out_channel =
            VirtChannel::new(ChannelDesc(ChannelType::Messages, ChannelDirection::Out));
        let trans_in_channel =
            VirtChannel::new(ChannelDesc(ChannelType::Transfer, ChannelDirection::In));
        let trans_out_channel =
            VirtChannel::new(ChannelDesc(ChannelType::Transfer, ChannelDirection::Out));

        // This code should place channels according to index values returned by `ChannelDesc::index` function.
        // We could use HashMap instead, but than we should handle case, when ChannelDesc is not in the array,
        // what pollutes code with unnecessary Results.
        // Assumption is checked by `test_virt_node_creation_assumption` test.
        let channels = [
            trans_in_channel,
            trans_out_channel,
            msg_in_channel,
            msg_out_channel,
        ];

        VirtNode {
            address: ip,
            routing,
            channels,
        }
    }

    pub fn id(&self) -> NodeId {
        self.routing.target()
    }

    pub async fn transition(
        &self,
        channel: ChannelDesc,
        new_state: TcpState,
    ) -> Result<(), TcpTransitionError> {
        let channel = self.channel(channel);
        let mut state = channel.state.write().await;
        state.transition(new_state)
    }

    pub fn notifier(
        &self,
        channel: ChannelDesc,
    ) -> Arc<broadcast::Sender<Result<Arc<TcpConnection>, TcpError>>> {
        self.channel(channel).state_notifier
    }

    pub fn channel(&self, channel: ChannelDesc) -> VirtChannel {
        self.channels[channel.index()].clone()
    }
}

#[derive(Clone)]
pub struct TcpRegistry {
    layer: SessionLayer,
    state: Arc<RwLock<TcpRegistryState>>,
}

struct TcpRegistryState {
    nodes: HashMap<Box<[u8]>, VirtNode>,
    ips: HashMap<NodeId, Box<[u8]>>,
}

impl TcpRegistry {
    pub fn new(layer: SessionLayer) -> TcpRegistry {
        TcpRegistry {
            layer,
            state: Arc::new(RwLock::new(TcpRegistryState {
                nodes: Default::default(),
                ips: Default::default(),
            })),
        }
    }

    pub async fn connect_attempt(&self, node: NodeId, channel: ChannelDesc) -> TcpLock {
        let node = match self.resolve_node(node).await {
            Ok(node) => node,
            Err(_) => self.add_virt_node(node).await,
        };

        let notifier = node.notifier(channel).subscribe();

        match node.transition(channel, TcpState::Connecting).await {
            Ok(_) => TcpLock::Permit(TcpPermit {
                node,
                channel,
                result: None,
            }),
            Err(_) => TcpLock::Wait(TcpAwaiting {
                notifier,
                channel: node.channel(channel),
            }),
        }
    }

    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        let state = self.state.read().await;
        let ip = state
            .ips
            .get(&node)
            .ok_or_else(|| anyhow!("Virtual node for id [{node}] not found."))?;
        state
            .nodes
            .get(ip)
            .cloned()
            .ok_or_else(|| anyhow!("Virtual node for ip {ip:?} not found."))
    }

    pub async fn resolve_ip(&self, node: NodeId) -> Box<[u8]> {
        IpAddress::from(to_ipv6(node.into_array()))
            .as_bytes()
            .into()
    }

    pub async fn remove_node(&self, node_id: NodeId) {
        // Removing all channels. Consider if we should remove channels one by one.
        if let Ok(node) = self.resolve_node(node_id).await {
            node.transition(
                (ChannelType::Messages, ChannelDirection::Out).into(),
                TcpState::Closed,
            )
            .await
            .on_err(|e| log::warn!("Tcp removing Node: {node_id} (Messages): {e}"))
            .ok();
            node.transition(
                (ChannelType::Transfer, ChannelDirection::Out).into(),
                TcpState::Closed,
            )
            .await
            .on_err(|e| log::warn!("Tcp removing Node: {node_id} (Transfer): {e}"))
            .ok();
        };

        // let mut state = self.state.write().await;
        // if let Some(remote_ip) = state.ips.remove(&node_id) {
        //     log::debug!("[VirtualTcp] Disconnected from node [{node_id}]");
        //     state.nodes.remove(&remote_ip);
        // }
    }

    pub async fn get_by_address(&self, addr: &[u8]) -> Option<VirtNode> {
        let state = self.state.read().await;
        state.nodes.get(addr).cloned()
    }

    pub async fn add_virt_node(&self, node_id: NodeId) -> VirtNode {
        let node = VirtNode::new(node_id, self.layer.clone());
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.address.as_bytes().into();

            state.nodes.insert(ip.clone(), node.clone());
            state.ips.insert(node_id, ip);
        }
        node
    }
}

#[derive(Clone, Educe, Display, Debug)]
#[educe(PartialEq)]
pub enum TcpState {
    Connecting,
    /// We store `Arc` not `Weak` here and this is the only place that keeps
    /// this reference. Changing state will drop the connection.
    /// TODO: Could we clean whole `TcpLayer` and `ya-relay-stack` state on drop??
    #[display(fmt = "Connected")]
    Connected(#[educe(PartialEq(ignore))] Arc<TcpConnection>),
    Failed(TcpError),
    Closing,
    Closed,
}

impl TcpState {
    pub fn transition(&mut self, new_state: TcpState) -> Result<(), TcpTransitionError> {
        match (&self, &new_state) {
            (&TcpState::Closed, &TcpState::Connecting)
            | (&TcpState::Failed(_), &TcpState::Connecting)
            | (&TcpState::Connecting, &TcpState::Failed(_))
            | (&TcpState::Connecting, &TcpState::Connected(_))
            | (&TcpState::Connected(_), &TcpState::Closing)
            | (&TcpState::Connected(_), &TcpState::Closed) => (),
            // | (&TcpState::Closing, &TcpState::Closed) => (),
            _ => {
                return Err(TcpTransitionError::InvalidTransition(
                    self.clone(),
                    new_state.clone(),
                ))
            }
        };
        *self = new_state;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct TcpConnection {
    pub id: NodeId,
    pub conn: Connection,
    pub channel: ChannelDesc,
}

/// Structure giving you exclusive right to initialize connection.
///
/// This duplicates `SessionPermit`, but I don't see an easy way to unify them
/// without over-engineering.
pub struct TcpPermit {
    pub node: VirtNode,
    channel: ChannelDesc,
    pub(crate) result: Option<Result<Arc<TcpConnection>, TcpError>>,
}

impl TcpPermit {
    pub fn finish(
        &mut self,
        result: Result<Arc<TcpConnection>, TcpError>,
    ) -> Result<Arc<TcpConnection>, TcpError> {
        self.result = Some(result.clone());
        result
    }
}

pub(crate) async fn async_drop(
    node: VirtNode,
    channel: ChannelDesc,
    result: Option<Result<Arc<TcpConnection>, TcpError>>,
) {
    let result = match result.clone() {
        Some(Ok(conn)) => {
            match node
                .transition(channel, TcpState::Connected(conn.clone()))
                .await
            {
                Ok(_) => Ok(conn),
                Err(e) => Err(TcpError::Generic(e.to_string())),
            }
        }
        Some(Err(e)) => {
            node.transition(channel, TcpState::Failed(e.clone()))
                .await
                .ok();
            Err(e)
        }
        None => Err(TcpError::Generic(
            "Dropping `TcpPermit` without result.".to_string(),
        )),
    };

    node.notifier(channel)
        .send(result)
        .map_err(|_e| {
            log::trace!(
                "No one was waiting for info about established tcp connection with [{}]",
                node.id()
            )
        })
        .ok();
}

impl Drop for TcpPermit {
    fn drop(&mut self) {
        log::trace!("Dropping `TcpPermit` for {}.", self.node.id());
        tokio::task::spawn_local(async_drop(
            self.node.clone(),
            self.channel,
            self.result.take(),
        ));
    }
}

/// Structure for awaiting established connection.
pub struct TcpAwaiting {
    channel: VirtChannel,
    notifier: broadcast::Receiver<Result<Arc<TcpConnection>, TcpError>>,
}

impl TcpAwaiting {
    pub async fn await_for_finish(&mut self) -> Result<Arc<TcpConnection>, TcpError> {
        match self.channel.state().await {
            TcpState::Connected(result) => return Ok(result),
            TcpState::Closed => return Err(TcpError::Closed),
            TcpState::Failed(e) => return Err(TcpError::Generic(e.to_string())),
            // In all other cases we will wait for notification.
            _ => (),
        };

        match self.notifier.recv().await {
            Ok(result) => result,
            Err(RecvError::Closed) => Err(TcpError::Generic(
                "Waiting for tcp connection initialization: notifier dropped.".to_string(),
            )),
            Err(RecvError::Lagged(lost)) => {
                // TODO: Maybe we could handle lags better, by checking current state?
                Err(TcpError::Generic(format!("Waiting for tcp connection initialization: notifier lagged. Lost {lost} state change(s).")))
            }
        }
    }
}

/// Structure giving you exclusive right to initialize connection (`Permit`)
/// or allows to wait for results (`Wait`).
pub enum TcpLock {
    Permit(TcpPermit),
    Wait(TcpAwaiting),
}

/// Interface for sending data to other Node using reliable transport.
///
/// If connection was closed, it will be re-initialized.
#[derive(Clone)]
pub struct TcpSender {
    pub(crate) target: NodeId,
    pub(crate) channel: ChannelDesc,
    pub(crate) connection: Weak<TcpConnection>,
    pub(crate) layer: TcpLayer,
}

impl TcpSender {
    /// Sends Payload to target Node using reliable transport.
    /// Creates connection if it doesn't exist or was closed.
    /// TODO: We should use channel-like error where you can recover your payload
    ///       from error message.
    pub async fn send(&mut self, packet: Payload) -> Result<(), TcpError> {
        let routing = match self.connection.upgrade() {
            Some(conn) => conn,
            None => match self
                .layer
                .connect(self.target, self.channel.0)
                .await
                .map_err(|e| TcpError::Generic(format!("Establishing connection failed: {e}")))?
                .connection
                .upgrade()
            {
                Some(routing) => {
                    self.connection = Arc::downgrade(&routing);
                    routing
                }
                None => {
                    return Err(TcpError::Generic(
                        "Tcp connection closed unexpectedly.".to_string(),
                    ))
                }
            },
        };
        self.layer
            .send(packet, routing.conn)
            .await
            .map_err(|e| TcpError::Generic(e.to_string()))
    }

    pub async fn connect(&mut self) -> Result<(), TcpError> {
        self.layer
            .connect(self.target, self.channel.0)
            .await
            .map_err(|e| TcpError::Generic(format!("Establishing connection failed: {e}")))?;
        Ok(())
    }

    /// Closes connection to Node. In case of relayed connection only forwarding information
    /// will be removed.
    pub async fn disconnect(&mut self) -> Result<(), TcpError> {
        self.layer.remove_node(self.target).await;
        Ok(())
    }
}

pub(crate) fn channel_endpoint(id: NodeId, channel: ChannelType) -> IpEndpoint {
    (to_ipv6(id), channel as u16).into()
}

pub(crate) fn to_ipv6(bytes: impl AsRef<[u8]>) -> Ipv6Addr {
    const IPV6_ADDRESS_LEN: usize = 16;

    let bytes = bytes.as_ref();
    let len = IPV6_ADDRESS_LEN.min(bytes.len());
    let mut ipv6_bytes = [0u8; IPV6_ADDRESS_LEN];

    // copy source bytes
    ipv6_bytes[..len].copy_from_slice(&bytes[..len]);
    // no multicast addresses
    ipv6_bytes[0] %= 0xff;
    // no unspecified or localhost addresses
    if ipv6_bytes[0..15] == [0u8; 15] && ipv6_bytes[15] < 0x02 {
        ipv6_bytes[15] = 0x02;
    }

    Ipv6Addr::from(ipv6_bytes)
}

impl From<(ChannelType, ChannelDirection)> for ChannelDesc {
    fn from(value: (ChannelType, ChannelDirection)) -> Self {
        ChannelDesc(value.0, value.1)
    }
}

impl ChannelDesc {
    pub fn index(&self) -> usize {
        match self {
            ChannelDesc(ChannelType::Transfer, ChannelDirection::In) => 0,
            ChannelDesc(ChannelType::Transfer, ChannelDirection::Out) => 1,
            ChannelDesc(ChannelType::Messages, ChannelDirection::In) => 2,
            ChannelDesc(ChannelType::Messages, ChannelDirection::Out) => 3,
        }
    }

    pub fn port(&self) -> u16 {
        self.0 as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::init::MockSessionNetwork;

    /// `VirtNode` code assumes that channels are residing under index, that can be returned
    /// by `ChannelDesc::index` function.
    #[actix_rt::test]
    async fn test_virt_node_creation_assumption() {
        let mut network = MockSessionNetwork::new().await.unwrap();
        let layer = network.new_layer().await.unwrap().layer;

        let virt = VirtNode::new(NodeId::default(), layer);
        for i in 0..4 {
            assert_eq!(virt.channels[i].channel.index(), i);
        }
    }
}
