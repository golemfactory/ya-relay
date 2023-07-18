use anyhow::{anyhow, Context};
use futures::channel::mpsc;
use futures::StreamExt;
use std::collections::HashMap;
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

use ya_relay_core::session::Session;
use ya_relay_proto::proto::{Forward, Payload, SlotId};
use ya_relay_stack::interface::{add_iface_address, add_iface_route, pcap_tun_iface, tun_iface};
use ya_relay_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT, TCP_DISCONN_TIMEOUT};
use ya_relay_stack::ya_smoltcp::iface::Route;
use ya_relay_stack::ya_smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_relay_stack::*;

use crate::legacy::client::Forwarded;
use crate::legacy::registry::NodeEntry;
use crate::legacy::ForwardReceiver;

const IPV6_DEFAULT_CIDR: u8 = 0;

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
    pub session: Arc<Session>,
    pub session_slot: SlotId,
}

/// Client implements TCP protocol over underlying UDP.
/// To use TCP we need to create virtual network, so that TCP stack appears to
/// connect to real IP addresses. This layer translates NodeIds into virtual IPs
/// and handles virtual connections.
#[derive(Clone)]
pub struct TcpLayer {
    net: Network,
    state: Arc<RwLock<TcpLayerState>>,
}

struct TcpLayerState {
    ingress: Channel<Forwarded>,

    nodes: HashMap<Box<[u8]>, VirtNode>,
    ips: HashMap<NodeId, Box<[u8]>>,
}

impl VirtNode {
    pub fn try_new(
        id: NodeId,
        session: Arc<Session>,
        session_slot: SlotId,
    ) -> anyhow::Result<Self> {
        let ip = IpAddress::from(to_ipv6(id.into_array()));
        let endpoint = (ip, PortType::Messages as u16).into();

        Ok(Self {
            id,
            endpoint,
            session,
            session_slot,
        })
    }
}

impl TcpLayer {
    pub fn new(key: &PublicKey, config: &StackConfig, ingress: &Channel<Forwarded>) -> TcpLayer {
        let pcap = config.pcap_path.clone().map(|p| match pcap_writer(p) {
            Ok(pcap) => pcap,
            Err(err) => panic!("{}", err),
        });
        let net = default_network(key.clone(), Rc::new(config.clone()), pcap);

        TcpLayer {
            net,
            state: Arc::new(RwLock::new(TcpLayerState {
                ingress: ingress.clone(),
                nodes: Default::default(),
                ips: Default::default(),
            })),
        }
    }

    fn net_id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn receiver(&self) -> Option<ForwardReceiver> {
        self.state.read().await.ingress.receiver()
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
        state.resolve_node(node)
    }

    pub async fn add_virt_node(&self, node: NodeEntry) -> anyhow::Result<VirtNode> {
        let node = VirtNode::try_new(node.id, node.session, node.slot)?;
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();

            state.nodes.insert(ip.clone(), node.clone());
            state.ips.insert(node.id, ip);
        }
        Ok(node)
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
            log::debug!("[VirtualTcp] Disconnected from node [{}]", node_id);
            state.nodes.remove(&remote_ip);
        }
        Ok(())
    }

    /// Connects to other Node and returns `Connection` for sending data.
    pub async fn connect(&self, node: NodeEntry, port: PortType) -> anyhow::Result<Connection> {
        log::debug!(
            "[VirtualTcp] Connecting to node [{}] using session {}.",
            node.id,
            node.session.id
        );

        print_sockets(&self.net);

        // This will override previous Node settings, if we had them.
        let node = self.add_virt_node(node).await?;
        let endpoint = IpEndpoint::new(node.endpoint.addr, port as u16);

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

    pub async fn get_next_fwd_payload<T>(
        &self,
        rx: &mut mpsc::Receiver<T>,
        forward_pause: &Actuator,
    ) -> Option<T> {
        if let Some(resumed) = forward_pause.next() {
            resumed.await;
        }
        rx.next().await
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

    pub async fn receive(&self, node: NodeEntry, payload: Payload) {
        ya_packet_trace::packet_trace_maybe!("TcpLayer::Receive", {
            &ya_packet_trace::try_extract_from_ip_frame(payload.as_ref())
        });

        if self.resolve_node(node.id).await.is_err() {
            log::debug!(
                "[VirtualTcp] Incoming message from new Node [{}]. Adding connection.",
                node.id
            );
            self.add_virt_node(node).await.ok();
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
                        // nodes are populated via `Client::on_forward` and `Client::forward`
                        let state = myself.state.read().await;
                        state
                            .nodes
                            .get(remote_address.as_bytes())
                            .map(|node| (node.id, state.ingress.tx.clone()))
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
                    let node = match {
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

                    let forward = Forward::new(node.session.id, node.session_slot, egress.payload);
                    if let Err(error) = node.session.send(forward).await {
                        log::trace!(
                            "[{}] egress router: forward to {} failed: {}",
                            myself.net_id(),
                            node.id,
                            error
                        );
                    }
                }
            })
            .await
    }
}

impl TcpLayerState {
    fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        let ip = self
            .ips
            .get(&node)
            .ok_or_else(|| anyhow!("Virtual node for id [{}] not found.", node))?;
        self.nodes
            .get(ip)
            .cloned()
            .ok_or_else(|| anyhow!("Virtual node for ip {:?} not found.", ip))
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
