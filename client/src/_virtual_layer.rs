#![allow(dead_code)]
#![allow(unused)]

use anyhow::{anyhow, Context};
use futures::channel::mpsc;
use futures::StreamExt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_core::crypto::PublicKey;
use ya_relay_core::server_session::TransportType;
use ya_relay_core::sync::Actuator;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::Payload;
use ya_relay_stack::interface::{add_iface_address, add_iface_route, pcap_tun_iface, tun_iface};
use ya_relay_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT, TCP_DISCONN_TIMEOUT};
use ya_relay_stack::ya_smoltcp::iface::Route;
use ya_relay_stack::ya_smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_relay_stack::*;

use crate::_client::Forwarded;
use crate::_routing_session::RoutingSender;
use crate::_session_layer::SessionLayer;
use crate::_transport_layer::ForwardReceiver;

const IPV6_DEFAULT_CIDR: u8 = 0;

// TODO: Try to unify with `TransportType`.
#[derive(Clone, Copy)]
pub enum PortType {
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

/// Client implements TCP protocol over underlying UDP.
/// To use TCP we need to create virtual network, so that TCP stack appears to
/// connect to real IP addresses. This layer translates NodeIds into virtual IPs
/// and handles virtual connections.
#[derive(Clone)]
pub struct TcpLayer {
    net: Network,
    session_layer: SessionLayer,

    state: Arc<RwLock<TcpLayerState>>,

    ingress: Channel<Forwarded>,
    virtual_tcp_fast_lane: Rc<RefCell<HashSet<NodeId>>>,
}

struct TcpLayerState {
    nodes: HashMap<Box<[u8]>, VirtNode>,
    ips: HashMap<NodeId, Box<[u8]>>,
}

impl TcpLayer {
    pub fn new(
        key: &PublicKey,
        config: &StackConfig,
        ingress: &Channel<Forwarded>,
        session_layer: SessionLayer,
    ) -> TcpLayer {
        let pcap = config.pcap_path.clone().map(|p| match pcap_writer(p) {
            Ok(pcap) => pcap,
            Err(err) => panic!("{}", err),
        });
        let net = default_network(key.clone(), Rc::new(config.clone()), pcap);

        TcpLayer {
            net,
            ingress: ingress.clone(),
            state: Arc::new(RwLock::new(TcpLayerState {
                nodes: Default::default(),
                ips: Default::default(),
            })),
            virtual_tcp_fast_lane: Rc::new(RefCell::new(Default::default())),
            session_layer,
        }
    }

    fn net_id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn receiver(&self) -> Option<ForwardReceiver> {
        self.ingress.receiver()
    }

    pub async fn spawn(&self, our_id: NodeId) -> anyhow::Result<()> {
        let virt_endpoint: IpEndpoint = (to_ipv6(our_id), PortType::Messages as u16).into();
        let virt_transfer_endpoint: IpEndpoint =
            (to_ipv6(our_id), PortType::Transfer as u16).into();

        self.net.spawn_local();
        self.net.bind(Protocol::Tcp, virt_endpoint)?;
        self.net.bind(Protocol::Tcp, virt_transfer_endpoint)?;

        self.spawn_ingress_router().await?;
        self.spawn_egress_router().await?;
        Ok(())
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

    pub fn new_virtual_node(&self, id: NodeId) -> VirtNode {
        let ip = IpAddress::from(to_ipv6(id.into_array()));
        let endpoint = (ip, PortType::Messages as u16).into();
        let session = RoutingSender::empty(id, self.session_layer.clone());

        VirtNode {
            id,
            endpoint,
            session,
        }
    }

    pub async fn add_virt_node(&self, node_id: NodeId) -> VirtNode {
        let node = self.new_virtual_node(node_id);
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();

            state.nodes.insert(ip.clone(), node.clone());
            state.ips.insert(node_id, ip);
        }
        node
    }

    pub async fn remove_node(&self, node_id: NodeId) -> anyhow::Result<()> {
        let remote_ip = {
            let state = self.state.read().await;
            match state.ips.get(&node_id).cloned() {
                Some(ip) => ip,
                _ => return Ok(()),
            }
        };

        self.net
            .disconnect_all(remote_ip, TCP_DISCONN_TIMEOUT)
            .await;

        let mut state = self.state.write().await;
        if let Some(remote_ip) = state.ips.remove(&node_id) {
            log::debug!("[VirtualTcp] Disconnected from node [{node_id}]");
            state.nodes.remove(&remote_ip);
        }
        Ok(())
    }

    /// Connects to other Node and returns `Connection` for sending data.
    /// TODO: We need to ensure that only one single connection can be established
    ///       at the same time and rest of attempts will wait for finish.
    /// TODO: Currently we create separate TCP connection for each identity on other Node.
    ///       Should we protect ourselves from this?
    pub async fn connect(&self, node_id: NodeId, port: PortType) -> anyhow::Result<Connection> {
        log::debug!("[VirtualTcp] Connecting to node [{node_id}].",);

        print_sockets(&self.net);

        // Make sure we have session with the Node. This allows us to
        // exit early if target Node is unreachable.
        self.new_virtual_node(node_id).session.connect().await?;

        let node = self.add_virt_node(node_id).await;
        let endpoint = IpEndpoint::new(node.endpoint.addr, port as u16);

        // TODO: Guard creating TCP connection. `connect` can be called from many
        //       functions at the same time.
        Ok(self.net.connect(endpoint, TCP_CONN_TIMEOUT).await?)
    }

    #[inline]
    pub fn sockets(&self) -> Vec<(SocketDesc, SocketState<ChannelMetrics>)> {
        self.net.sockets()
    }

    #[inline]
    pub fn metrics(&self) -> ChannelMetrics {
        self.net.metrics()
    }

    #[inline(always)]
    pub async fn send(
        &self,
        data: impl Into<Payload>,
        connection: Connection,
    ) -> anyhow::Result<()> {
        let data: Payload = data.into();

        ya_packet_trace::packet_trace_maybe!("TcpLayer::Send", {
            &ya_packet_trace::try_extract_from_ip_frame(data.as_ref())
        });

        Ok(self.net.send(data, connection).await?)
    }

    pub async fn dispatch(&self, packet: Forwarded) {
        let node_id = packet.node_id;
        let exists = {
            // Optimisation to avoid resolving Node if possible.
            let fast_lane = self.virtual_tcp_fast_lane.borrow();
            fast_lane.contains(&node_id)
        };

        if exists {
            self.inject(packet.payload);
            return;
        }

        self.receive(node_id, packet.payload).await;
        self.virtual_tcp_fast_lane.borrow_mut().insert(node_id);
    }

    pub async fn receive(&self, node: NodeId, payload: Payload) {
        ya_packet_trace::packet_trace_maybe!("TcpLayer::Receive", {
            &ya_packet_trace::try_extract_from_ip_frame(payload.as_ref())
        });

        if self.resolve_node(node).await.is_err() {
            log::debug!("[VirtualTcp] Incoming message from new Node [{node}]. Adding connection.");
            self.add_virt_node(node).await;
        }
        self.inject(payload);
    }

    #[inline]
    pub fn inject(&self, payload: Payload) {
        self.net.receive(payload);
        self.net.poll();
    }

    pub async fn shutdown(&self) {
        // empty
    }

    async fn spawn_ingress_router(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .net
            .ingress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Ingress traffic router already spawned"))?;

        tokio::task::spawn_local(self.clone().ingress_router(ingress_rx));
        Ok(())
    }

    async fn spawn_egress_router(&self) -> anyhow::Result<()> {
        let egress_rx = self
            .net
            .egress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Egress traffic router already spawned"))?;

        tokio::task::spawn_local(self.clone().egress_router(egress_rx));
        Ok(())
    }

    async fn ingress_router(self, ingress_rx: UnboundedReceiver<IngressEvent>) {
        UnboundedReceiverStream::new(ingress_rx)
            .for_each(move |event| {
                let myself = self.clone();
                async move {
                    let (desc, payload) = match event {
                        IngressEvent::InboundConnection { desc } => {
                            log::trace!(
                                "[{}] ingress router: new connection from {:?} to {:?} ",
                                myself.net_id(),
                                desc.remote,
                                desc.local,
                            );
                            return;
                        }
                        IngressEvent::Disconnected { desc } => {
                            log::trace!(
                                "[{}] ingress router: ({}) {:?} disconnected from {:?}",
                                myself.net_id(),
                                desc.protocol,
                                desc.remote,
                                desc.local,
                            );
                            return;
                        }
                        IngressEvent::Packet { desc, payload, .. } => {
                            ya_packet_trace::packet_trace_maybe!("TcpLayer::ingress_router", {
                                &ya_packet_trace::try_extract_from_ip_frame(&payload)
                            });

                            (desc, payload)
                        }
                    };

                    if desc.protocol != Protocol::Tcp {
                        log::trace!(
                            "[{}] ingress router: dropping {} payload",
                            myself.net_id(),
                            desc.protocol
                        );
                        return;
                    }

                    let (remote_address, local_port) = match (desc.remote, desc.local) {
                        (SocketEndpoint::Ip(remote), SocketEndpoint::Ip(local)) => {
                            (remote.addr, local.port)
                        }
                        _ => {
                            log::trace!(
                                "[{}] ingress router: remote endpoint {:?} is not supported",
                                myself.net_id(),
                                desc.remote
                            );
                            return;
                        }
                    };

                    match {
                        // Nodes are populated via `VirtualLayer::dispatch`
                        let state = myself.state.read().await;
                        state
                            .nodes
                            .get(remote_address.as_bytes())
                            .map(|node| (node.id, myself.ingress.tx.clone()))
                    } {
                        Some((node_id, tx)) => {
                            let payload_len = payload.len();
                            let payload = Forwarded {
                                transport: match PortType::from(local_port) {
                                    PortType::Messages => TransportType::Reliable,
                                    PortType::Transfer => TransportType::Transfer,
                                },
                                node_id,
                                payload: payload.into(),
                            };

                            if tx.send(payload).is_err() {
                                log::trace!(
                                    "[{}] ingress router: ingress handler closed for node {node_id}",
                                    myself.net_id()
                                );
                            } else {
                                log::trace!(
                                    "[{}] ingress router: forwarded {payload_len} B",
                                    myself.net_id()
                                );
                            }
                        }
                        _ => log::trace!(
                            "[{}] ingress router: unknown remote address {remote_address}",
                            myself.net_id()
                        ),
                    };
                }
            })
            .await
    }

    async fn egress_router(self, egress_rx: UnboundedReceiver<EgressEvent>) {
        UnboundedReceiverStream::new(egress_rx)
            .for_each(move |egress| {
                let myself = self.clone();
                async move {
                    let mut node = match {
                        let state = myself.state.read().await;
                        state.nodes.get(&egress.remote).cloned()
                    } {
                        Some(node) => node,
                        None => {
                            log::trace!(
                                "[{}] egress router: unknown address {:02x?}",
                                myself.net_id(),
                                egress.remote
                            );
                            return;
                        }
                    };

                    // `RoutingSender::send` will lazily create session with target Node.
                    // In most cases session will exist, but if not, we need to protect from
                    // blocking the rest of packets in the stream.
                    //
                    // Note that thanks to lazy sessions, even if we have unstable connection,
                    // TCP sessions are able to survive disconnection on lower layer. In previous
                    // implementation we disconnected TCP and all GSB messages in queue were lost.
                    tokio::task::spawn_local(async move {
                        if let Err(error) = node
                            .session
                            .send(egress.payload.into(), TransportType::Reliable)
                            .await
                        {
                            // TODO: In case of failure it would be nice to somehow send this error
                            //       back to message sender. In current scenario GSB messages will
                            //       wait until timeout. This makes error messages from this library
                            //       really poor, because everything from outside looks like a timeout.
                            log::trace!(
                                "[{}] egress router: forward to {} failed: {}",
                                myself.net_id(),
                                node.id,
                                error
                            );
                            // TODO: Should we close TCP connection here?
                        }
                    });
                }
            })
            .await
    }
}

fn to_ipv6(bytes: impl AsRef<[u8]>) -> Ipv6Addr {
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

fn pcap_writer(path: PathBuf) -> anyhow::Result<Box<dyn Write>> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!(format!(
            "Unable to read pcap file parent directory: {}",
            path.display()
        ))
    })?;

    std::fs::create_dir_all(parent).context(format!(
        "Unable to create pcap file parent directory: {}",
        parent.display()
    ))?;

    let file = std::fs::File::create(&path).context(format!(
        "Unable to open pcap file for writing: {}",
        path.display()
    ))?;

    let pcap = Box::new(std::io::BufWriter::new(file));
    Ok(pcap)
}

fn default_network(
    key: PublicKey,
    config: Rc<StackConfig>,
    pcap: Option<Box<dyn Write>>,
) -> Network {
    let address = key.address();
    let ipv6_addr = to_ipv6(address);
    let ipv6_cidr = IpCidr::new(IpAddress::from(ipv6_addr), IPV6_DEFAULT_CIDR);
    let mut iface = match pcap {
        Some(pcap) => pcap_tun_iface(config.max_transmission_unit, pcap),
        None => tun_iface(config.max_transmission_unit),
    };

    let name = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        address[0], address[1], address[2], address[3]
    );

    log::debug!("[{}] IP address: {}", name, ipv6_addr);

    add_iface_address(&mut iface, ipv6_cidr);
    add_iface_route(
        &mut iface,
        ipv6_cidr,
        Route::new_ipv6_gateway(ipv6_addr.into()),
    );

    Network::new(name, config.clone(), Stack::new(iface, config))
}

impl From<u16> for PortType {
    fn from(port: u16) -> Self {
        if port == PortType::Messages as u16 {
            PortType::Messages
        } else if port == PortType::Transfer as u16 {
            PortType::Transfer
        } else {
            PortType::Messages
        }
    }
}

pub fn print_sockets(network: &Network) {
    log::debug!("[inet] existing sockets:");
    for (handle, meta, state) in network.sockets_meta() {
        log::debug!("[inet] socket: {handle} ({}) {meta}", state.to_string());
    }
    log::debug!("[inet] existing connections:");
    for (handle, meta) in network.handles.borrow_mut().iter() {
        log::debug!("[inet] connection: {handle} {meta}");
    }

    log::debug!("[inet] listening sockets:");
    for handle in network.bindings.borrow_mut().iter() {
        log::debug!("[inet] listening socket: {handle}");
    }
}
