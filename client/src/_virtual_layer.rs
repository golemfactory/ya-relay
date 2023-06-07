use anyhow::Context;
use futures::StreamExt;
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_core::crypto::PublicKey;
use ya_relay_core::server_session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::Payload;
use ya_relay_stack::interface::{add_iface_address, add_iface_route, pcap_tun_iface, tun_iface};
use ya_relay_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT, TCP_DISCONN_TIMEOUT};
use ya_relay_stack::ya_smoltcp::iface::Route;
use ya_relay_stack::ya_smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_relay_stack::{
    Channel, ChannelMetrics, Connection, EgressEvent, IngressEvent, Network, Protocol, SocketDesc,
    SocketState, Stack, StackConfig,
};

use crate::_client::Forwarded;
use crate::_error::TcpError;
use crate::_session_layer::SessionLayer;
use crate::_tcp_registry::{
    to_ipv6, ChannelType, TcpConnection, TcpLock, TcpPermit, TcpRegistry, TcpSender, VirtNode,
};
use crate::_transport_layer::ForwardReceiver;

const IPV6_DEFAULT_CIDR: u8 = 0;

/// Client implements TCP protocol over underlying UDP.
/// To use TCP we need to create virtual network, so that TCP stack appears to
/// connect to real IP addresses. This layer translates NodeIds into virtual IPs
/// and handles virtual connections.
#[derive(Clone)]
pub struct TcpLayer {
    pub(crate) net: Network,
    session_layer: SessionLayer,

    registry: TcpRegistry,

    ingress: Channel<Forwarded>,
    virtual_tcp_fast_lane: Rc<RefCell<HashSet<NodeId>>>,
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
            registry: TcpRegistry::new(session_layer.clone()),
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
        let virt_endpoint: IpEndpoint = (to_ipv6(our_id), ChannelType::Messages as u16).into();
        let virt_transfer_endpoint: IpEndpoint =
            (to_ipv6(our_id), ChannelType::Transfer as u16).into();

        self.net.spawn_local();
        self.net.bind(Protocol::Tcp, virt_endpoint)?;
        self.net.bind(Protocol::Tcp, virt_transfer_endpoint)?;

        self.spawn_ingress_router().await?;
        self.spawn_egress_router().await?;
        Ok(())
    }

    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        self.registry.resolve_node(node).await
    }

    pub async fn remove_node(&self, node_id: NodeId) -> anyhow::Result<()> {
        let remote_ip = self.registry.resolve_ip(node_id).await;

        self.net
            .disconnect_all(remote_ip, TCP_DISCONN_TIMEOUT)
            .await;

        self.registry.remove_node(node_id).await;
        Ok(())
    }

    /// Connects to other Node and returns `TcpSender` for sending data.
    /// TODO: We need to ensure that only one single connection can be established
    ///       at the same time and rest of attempts will wait for finish.
    /// TODO: Currently we create separate TCP connection for each identity on other Node.
    ///       Should we protect ourselves from this?
    pub async fn connect(
        &self,
        node_id: NodeId,
        channel: ChannelType,
    ) -> anyhow::Result<TcpSender> {
        print_sockets(&self.net);

        let myself = self.clone();
        let connection = match self.registry.connect_attempt(node_id, channel).await {
            TcpLock::Permit(mut permit) => {
                log::debug!("[VirtualTcp] Connecting to node [{node_id}], channel: {channel}.");

                // Spawning task protects us from dropping future during initialization.
                tokio::task::spawn_local(async move {
                    permit.finish(myself.connect_internal(channel, &permit).await)
                })
                .await?
            }
            TcpLock::Wait(mut waiter) => waiter.await_for_finish().await,
        }?;

        Ok(TcpSender {
            target: connection.id,
            channel,
            connection: Arc::downgrade(&connection),
            layer: self.clone(),
        })
    }

    async fn connect_internal(
        &self,
        channel: ChannelType,
        permit: &TcpPermit,
    ) -> Result<Arc<TcpConnection>, TcpError> {
        let endpoint = IpEndpoint::new(permit.node.address, channel as u16);

        // Make sure we have session with the Node. This allows us to
        // exit early if target Node is unreachable.
        self.session_layer
            .session(permit.node.id())
            .await
            .map_err(|e| TcpError::Generic(e.to_string()))?;

        Ok(Arc::new(TcpConnection {
            id: permit.node.id(),
            conn: self
                .net
                .connect(endpoint, TCP_CONN_TIMEOUT)
                .await
                .map_err(|e| TcpError::Generic(format!("Establishing Tcp connection: {e}")))?,
            channel,
        }))
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

        if self.registry.resolve_node(node).await.is_err() {
            log::debug!("[VirtualTcp] Incoming message from new Node [{node}]. Adding connection.");
            self.registry.add_virt_node(node).await;
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
                                "[{}] new tcp connection from {:?} to {:?} ",
                                myself.net_id(),
                                desc.remote,
                                desc.local,
                            );
                            return;
                        }
                        IngressEvent::Disconnected { desc } => {
                            log::trace!(
                                "[{}] virtual tcp: ({}) {:?} disconnected from {:?}",
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
                        myself.registry.get_by_address(remote_address.as_bytes()).await
                            .map(|node| (node.id(), myself.ingress.tx.clone()))
                    } {
                        Some((node_id, tx)) => {
                            let payload_len = payload.len();
                            let payload = Forwarded {
                                transport: match ChannelType::from(local_port) {
                                    ChannelType::Messages => TransportType::Reliable,
                                    ChannelType::Transfer => TransportType::Transfer,
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
                    let mut node = match myself.registry.get_by_address(&egress.remote).await {
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
                            .routing
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
                                node.id(),
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

impl From<u16> for ChannelType {
    fn from(port: u16) -> Self {
        if port == ChannelType::Messages as u16 {
            ChannelType::Messages
        } else if port == ChannelType::Transfer as u16 {
            ChannelType::Transfer
        } else {
            ChannelType::Messages
        }
    }
}

pub fn print_sockets(network: &Network) {
    log::trace!("[inet] existing sockets:");
    for (handle, meta, state) in network.sockets_meta() {
        log::trace!("[inet] socket: {handle} ({}) {meta}", state.to_string());
    }
    log::trace!("[inet] existing connections:");
    for (handle, meta) in network.handles.borrow_mut().iter() {
        log::trace!("[inet] connection: {handle} {meta}");
    }

    log::trace!("[inet] listening sockets:");
    for handle in network.bindings.borrow_mut().iter() {
        log::trace!("[inet] listening socket: {handle}");
    }
}
