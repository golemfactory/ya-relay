use anyhow::anyhow;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::StreamExt;
use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::Arc;
use tokio::sync::RwLock;

use ya_client_model::NodeId;
use ya_relay_core::crypto::PublicKey;
use ya_relay_core::utils::parse_node_id;

use ya_relay_proto::proto::{Forward, Payload, SlotId};
use ya_relay_stack::interface::{add_iface_address, add_iface_route, default_iface};
use ya_relay_stack::smoltcp::iface::Route;
use ya_relay_stack::smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_relay_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT};
use ya_relay_stack::{Channel, IngressEvent, Network, Protocol, Stack};

use crate::client::ClientConfig;
use crate::client::{ForwardSender, Forwarded};
use crate::registry::NodeEntry;
use crate::session::Session;
use crate::ForwardReceiver;

const TCP_BIND_PORT: u16 = 1;
const IPV6_DEFAULT_CIDR: u8 = 0;

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
    pub fn try_new(id: &[u8], session: Arc<Session>, session_slot: SlotId) -> anyhow::Result<Self> {
        let id = parse_node_id(id)?;
        let ip = IpAddress::from(to_ipv6(&id));
        let endpoint = (ip, TCP_BIND_PORT).into();

        Ok(Self {
            id,
            endpoint,
            session,
            session_slot,
        })
    }
}

impl TcpLayer {
    pub fn new(config: Arc<ClientConfig>, ingress: Channel<Forwarded>) -> TcpLayer {
        let net = default_network(config.node_pub_key.clone());
        TcpLayer {
            net,
            state: Arc::new(RwLock::new(TcpLayerState {
                ingress,
                nodes: Default::default(),
                ips: Default::default(),
            })),
        }
    }

    pub fn id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn receiver(&self) -> Option<ForwardReceiver> {
        self.state.read().await.ingress.receiver()
    }

    pub fn spawn(&self, our_id: NodeId) -> anyhow::Result<()> {
        let virt_endpoint: IpEndpoint = (to_ipv6(&our_id), TCP_BIND_PORT).into();

        self.net.spawn_local();
        self.net.bind(Protocol::Tcp, virt_endpoint)?;

        self.spawn_ingress_router()?;
        self.spawn_egress_router()?;
        Ok(())
    }

    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        let state = self.state.read().await;
        state.resolve_node(node)
    }

    pub async fn add_virt_node(&self, node: NodeEntry) -> anyhow::Result<VirtNode> {
        let node = VirtNode::try_new(&node.id.into_array(), node.session, node.slot)?;
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();

            state.nodes.insert(ip.clone(), node.clone());
            state.ips.insert(node.id, ip);
        }
        Ok(node)
    }

    pub async fn remove_node(&self, node_id: NodeId) -> anyhow::Result<()> {
        let mut state = self.state.write().await;

        if let Some(ip) = state.ips.remove(&node_id) {
            state.nodes.remove(&ip);
        }
        Ok(())
    }

    /// Connects to other Node and returns `Sink` for sending data
    /// and channel that will notify us, when connection will be broken.
    pub async fn connect(
        &self,
        node: NodeEntry,
    ) -> anyhow::Result<(ForwardSender, oneshot::Receiver<()>)> {
        // This will override previous Node settings, if we had them.
        let node = self.add_virt_node(node).await?;
        let connection = self.net.connect(node.endpoint, TCP_CONN_TIMEOUT).await?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let (disconnect_tx, disconnect_rx) = oneshot::channel();

        let id = self.id();
        let myself = self.clone();

        tokio::task::spawn_local(async move {
            log::trace!("Forwarding messages to {}", node.id);

            while let Some(payload) = rx.next().await {
                let _ = myself
                    .net
                    .send(payload, connection)
                    .unwrap_or_else(|e| Box::pin(futures::future::err(e)))
                    .await
                    .map_err(|e| {
                        log::warn!(
                            "[{}] unable to forward via {}: {}",
                            id,
                            node.session.remote,
                            e
                        )
                    });
            }

            // Cleanup all internal info about Node.
            myself
                .remove_node(node.id)
                .await
                .map_err(|e| log::warn!("TcpLayer - error removing node {}: {}", node.id, e))
                .ok();

            disconnect_tx.send(()).ok();
            rx.close();

            log::trace!(
                "[{}] forward: disconnected from server: {}",
                id,
                node.session.remote
            );
        });

        Ok((tx, disconnect_rx))
    }

    pub async fn receive(&self, node: NodeEntry, payload: Payload) {
        if let Err(_) = self.resolve_node(node.id).await {
            self.add_virt_node(node).await.ok();
        }

        self.net.receive(payload.into_vec());
        self.net.poll();
    }

    fn spawn_ingress_router(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .net
            .ingress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Ingress traffic router already spawned"))?;

        let myself = self.clone();
        tokio::task::spawn_local(ingress_rx.for_each(move |event| {
            let myself = myself.clone();
            async move {
                let (desc, payload) = match event {
                    IngressEvent::InboundConnection { desc } => {
                        log::trace!(
                            "[{}] ingress router: new connection from {:?} to {:?} ",
                            myself.id(),
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Disconnected { desc } => {
                        log::trace!(
                            "[{}] ingress router: ({}) {:?} disconnected from {:?}",
                            myself.id(),
                            desc.protocol,
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Packet { desc, payload, .. } => (desc, payload),
                };

                if desc.protocol != Protocol::Tcp {
                    log::trace!(
                        "[{}] ingress router: dropping {} payload",
                        myself.id(),
                        desc.protocol
                    );
                    return;
                }

                let remote_address = match desc.remote {
                    SocketEndpoint::Ip(endpoint) => endpoint.addr,
                    _ => {
                        log::trace!(
                            "[{}] ingress router: remote endpoint {:?} is not supported",
                            myself.id(),
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
                        let payload = Forwarded {
                            reliable: true,
                            node_id,
                            payload,
                        };

                        let payload_len = payload.payload.len();

                        if tx.send(payload).is_err() {
                            log::trace!(
                                "[{}] ingress router: ingress handler closed for node {}",
                                myself.id(),
                                node_id
                            );
                        } else {
                            log::trace!(
                                "[{}] ingress router: forwarded {} B",
                                myself.id(),
                                payload_len
                            );
                        }
                    }
                    _ => log::trace!(
                        "[{}] ingress router: unknown remote address {}",
                        myself.id(),
                        remote_address
                    ),
                };
            }
        }));

        Ok(())
    }

    fn spawn_egress_router(&self) -> anyhow::Result<()> {
        let egress_rx = self
            .net
            .egress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Egress traffic router already spawned"))?;

        let myself = self.clone();
        tokio::task::spawn_local(egress_rx.for_each(move |egress| {
            let myself = myself.clone();
            async move {
                let node = match {
                    let state = myself.state.read().await;
                    state.nodes.get(&egress.remote).cloned()
                } {
                    Some(node) => node,
                    None => {
                        log::trace!(
                            "[{}] egress router: unknown address {:02x?}",
                            myself.id(),
                            egress.remote
                        );
                        return;
                    }
                };

                let forward = Forward::new(node.session.id, node.session_slot, egress.payload);
                if let Err(error) = node.session.send(forward).await {
                    log::trace!(
                        "[{}] egress router: forward to {} failed: {}",
                        myself.id(),
                        node.id,
                        error
                    );
                }
            }
        }));

        Ok(())
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

fn default_network(key: PublicKey) -> Network {
    let address = key.address();
    let ipv6_addr = to_ipv6(address);
    let ipv6_cidr = IpCidr::new(IpAddress::from(ipv6_addr), IPV6_DEFAULT_CIDR);
    let mut iface = default_iface();

    let name = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        address[0], address[1], address[2], address[3]
    );

    log::debug!("[{}] Ethernet address: {}", name, iface.ethernet_addr());
    log::debug!("[{}] IP address: {}", name, ipv6_addr);

    add_iface_address(&mut iface, ipv6_cidr);
    add_iface_route(
        &mut iface,
        ipv6_cidr,
        Route::new_ipv6_gateway(ipv6_addr.into()),
    );

    Network::new(name, Stack::with(iface))
}
