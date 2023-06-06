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
use ya_relay_stack::ya_smoltcp::wire::IpAddress;
use ya_relay_stack::Connection;

use crate::_error::{ResultExt, TcpError, TcpTransitionError};
use crate::_routing_session::RoutingSender;
use crate::_session_layer::SessionLayer;
use crate::_virtual_layer::TcpLayer;

// TODO: Try to unify with `TransportType`.
// TODO: We could support any number of channels. User of the library could decide.
#[derive(Clone, Copy, Display, Debug)]
pub enum ChannelType {
    Messages = 1,
    Transfer = 2,
}

/// Information about virtual node in TCP network built over UDP protocol.
#[derive(Clone)]
pub struct VirtNode {
    pub address: IpAddress,
    pub routing: RoutingSender,

    /// We could support arbitrary number of channels, but this would require
    /// having dynamic data structure with locks, what is not worth at this moment.
    pub transfer: VirtChannel,
    pub message: VirtChannel,
}

#[derive(Clone)]
pub struct VirtChannel {
    pub channel: ChannelType,
    pub state: Arc<RwLock<TcpState>>,
    pub state_notifier: Arc<broadcast::Sender<Result<Arc<TcpConnection>, TcpError>>>,
}

impl VirtNode {
    pub fn new(id: NodeId, layer: SessionLayer) -> VirtNode {
        let ip = IpAddress::from(to_ipv6(id.into_array()));
        let routing = RoutingSender::empty(id, layer.clone());

        VirtNode {
            address: ip,
            routing,
            transfer: VirtChannel {
                channel: ChannelType::Transfer,
                state: Arc::new(RwLock::new(TcpState::Closed)),
                state_notifier: Arc::new(broadcast::channel(1).0),
            },
            message: VirtChannel {
                channel: ChannelType::Messages,
                state: Arc::new(RwLock::new(TcpState::Closed)),
                state_notifier: Arc::new(broadcast::channel(1).0),
            },
        }
    }

    pub fn id(&self) -> NodeId {
        self.routing.target()
    }

    pub async fn transition(
        &self,
        channel: ChannelType,
        new_state: TcpState,
    ) -> Result<(), TcpTransitionError> {
        let state = match channel {
            ChannelType::Messages => self.message.state.clone(),
            ChannelType::Transfer => self.message.state.clone(),
        };

        let mut state = state.write().await;
        state.transition(new_state)
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

    pub async fn connect_attempt(&self, node: NodeId, channel: ChannelType) -> TcpLock {
        let node = match self.resolve_node(node).await {
            Ok(node) => node,
            Err(_) => self.add_virt_node(node).await,
        };

        let notifier = match channel {
            ChannelType::Messages => node.message.state_notifier.subscribe(),
            ChannelType::Transfer => node.message.state_notifier.subscribe(),
        };

        match node.transition(channel, TcpState::Connecting).await {
            Ok(_) => TcpLock::Permit(TcpPermit {
                node,
                channel,
                result: None,
            }),
            Err(_) => TcpLock::Wait(TcpAwaiting { notifier }),
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
        IpAddress::from(to_ipv6(&node.into_array()))
            .as_bytes()
            .into()
    }

    pub async fn remove_node(&self, node_id: NodeId) {
        // Removing all channels. Consider if we should remove channels one by one.
        match self.resolve_node(node_id).await {
            Ok(node) => {
                node.transition(ChannelType::Messages, TcpState::Closed)
                    .await
                    .on_err(|e| log::warn!("Tcp removing Node: {node_id} (Messages): {e}"))
                    .ok();
                node.transition(ChannelType::Transfer, TcpState::Closed)
                    .await
                    .on_err(|e| log::warn!("Tcp removing Node: {node_id} (Transfer): {e}"))
                    .ok();
            }
            Err(_) => {}
        };

        let mut state = self.state.write().await;
        if let Some(remote_ip) = state.ips.remove(&node_id) {
            log::debug!("[VirtualTcp] Disconnected from node [{node_id}]");
            state.nodes.remove(&remote_ip);
        }
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
    Failed,
    Closed,
}

impl TcpState {
    pub fn transition(&mut self, new_state: TcpState) -> Result<(), TcpTransitionError> {
        match (&self, &new_state) {
            (&TcpState::Closed, &TcpState::Connecting)
            | (&TcpState::Failed, &TcpState::Connecting)
            | (&TcpState::Connecting, &TcpState::Failed)
            | (&TcpState::Connecting, &TcpState::Connected(_))
            | (&TcpState::Connected(_), &TcpState::Closed) => (),
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
    pub channel: ChannelType,
}

pub struct TcpPermit {
    pub node: VirtNode,
    channel: ChannelType,
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
    channel: ChannelType,
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
            node.transition(channel, TcpState::Failed).await.ok();
            Err(e)
        }
        None => Err(TcpError::Generic(
            "Dropping `TcpPermit` without result.".to_string(),
        )),
    };

    match channel {
        ChannelType::Messages => node.message.state_notifier.clone(),
        ChannelType::Transfer => node.message.state_notifier.clone(),
    }
    .send(result.clone())
    .map_err(|_e| log::warn!("Failed to send connection finished broadcast"))
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
    notifier: broadcast::Receiver<Result<Arc<TcpConnection>, TcpError>>,
}

impl TcpAwaiting {
    pub async fn await_for_finish(&mut self) -> Result<Arc<TcpConnection>, TcpError> {
        return match self.notifier.recv().await {
            Ok(result) => result,
            Err(RecvError::Closed) => Err(TcpError::Generic(
                "Waiting for tcp connection initialization: notifier dropped.".to_string(),
            )),
            Err(RecvError::Lagged(lost)) => {
                // TODO: Maybe we could handle lags better, by checking current state?
                Err(TcpError::Generic(format!("Waiting for tcp connection initialization: notifier lagged. Lost {lost} state change(s).")))
            }
        };
    }
}

/// Structure giving you exclusive right to initialize connection (`Permit`)
/// or allows to wait for results (`Wait`).
pub enum TcpLock {
    Permit(TcpPermit),
    Wait(TcpAwaiting),
}

#[derive(Clone)]
pub struct TcpSender {
    pub(crate) target: NodeId,
    pub(crate) channel: ChannelType,
    pub(crate) connection: Weak<TcpConnection>,
    pub(crate) layer: TcpLayer,
}

impl TcpSender {
    /// Sends Payload to target Node using reliable transport.
    /// Creates connection if it doesn't exist or was closed.
    pub async fn send(&mut self, packet: Payload) -> Result<(), TcpError> {
        let routing = match self.connection.upgrade() {
            Some(conn) => conn,
            None => match self
                .layer
                .connect(self.target, self.channel)
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
            .send(packet, routing.conn.clone())
            .await
            .map_err(|e| TcpError::Generic(e.to_string()))
    }
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
