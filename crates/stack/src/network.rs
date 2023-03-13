use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::{Either, LocalBoxFuture};
use futures::{Future, FutureExt, SinkExt, StreamExt, TryFutureExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::spawn_local;
use tokio::time::MissedTickBehavior;
use ya_smoltcp::iface::SocketHandle;
use ya_smoltcp::wire::IpEndpoint;

use crate::connection::{Connection, ConnectionMeta};
use crate::packet::{
    ip_ntoh, ArpField, ArpPacket, EtherFrame, IpPacket, PeekPacket, TcpPacket, UdpPacket,
};
use crate::protocol::Protocol;
use crate::socket::{SocketDesc, SocketEndpoint, SocketExt, SocketMemory, SocketState};
use crate::stack::Stack;
use crate::{ChannelMetrics, Error, Result};

use ya_relay_util::Payload;

pub const PCAP_FILE_ENV_VAR: &str = "YA_NET_PCAP_FILE";
pub const STACK_POLL_MS_ENV_VAR: &str = "YA_NET_STACK_POLL_MS";
pub const STACK_POLL_SENT_ENV_VAR: &str = "YA_NET_STACK_POLL_SENT_BATCH";
pub const STACK_POLL_RECV_ENV_VAR: &str = "YA_NET_STACK_POLL_RECV_BATCH";

const DEFAULT_POLL_SENT_BATCH: usize = 16348;
const DEFAULT_POLL_RECV_BATCH: usize = 32768;
const MIN_STACK_POLL_SENT_BATCH: usize = 2048;
const MIN_STACK_POLL_RECV_BATCH: usize = 4096;

pub type IngressReceiver = UnboundedReceiver<IngressEvent>;
pub type EgressReceiver = UnboundedReceiver<EgressEvent>;

#[derive(Clone)]
pub struct StackConfig {
    pub pcap_path: Option<PathBuf>,
    pub max_transmission_unit: usize,
    pub max_send_batch: usize,
    pub max_recv_batch: usize,
    pub tcp_mem: SocketMemory,
    pub udp_mem: SocketMemory,
    pub icmp_mem: SocketMemory,
    pub raw_mem: SocketMemory,
}

impl Default for StackConfig {
    fn default() -> Self {
        let max_send_batch = std::env::var(STACK_POLL_SENT_ENV_VAR)
            .and_then(|s| {
                s.parse::<usize>()
                    .map_err(|_| std::env::VarError::NotPresent)
            })
            .unwrap_or(DEFAULT_POLL_SENT_BATCH)
            .max(MIN_STACK_POLL_SENT_BATCH);

        let max_recv_batch = std::env::var(STACK_POLL_RECV_ENV_VAR)
            .and_then(|s| {
                s.parse::<usize>()
                    .map_err(|_| std::env::VarError::NotPresent)
            })
            .unwrap_or(DEFAULT_POLL_RECV_BATCH)
            .max(MIN_STACK_POLL_RECV_BATCH);

        Self {
            pcap_path: std::env::var(PCAP_FILE_ENV_VAR).ok().map(PathBuf::from),
            max_transmission_unit: 1400,
            max_send_batch,
            max_recv_batch,
            tcp_mem: SocketMemory::default_tcp(),
            udp_mem: SocketMemory::default_udp(),
            icmp_mem: SocketMemory::default_icmp(),
            raw_mem: SocketMemory::default_raw(),
        }
    }
}

#[derive(Clone)]
pub struct Network {
    pub name: Rc<String>,
    pub config: Rc<StackConfig>,
    pub stack: Stack<'static>,
    is_tun: bool,
    sender: StackSender,
    poller: StackPoller,
    /// Set of listening sockets. Socket is removed from this set, when connection is created.
    /// Network stack will create new binding using new handle in place of previous.
    pub bindings: Rc<RefCell<HashSet<SocketHandle>>>,
    pub connections: Rc<RefCell<HashMap<ConnectionMeta, Connection>>>,
    pub handles: Rc<RefCell<HashMap<SocketHandle, ConnectionMeta>>>,
    ingress: Channel<IngressEvent>,
    egress: Channel<EgressEvent>,
}

impl Network {
    /// Creates a new Network instance
    pub fn new(name: impl ToString, config: Rc<StackConfig>, stack: Stack<'static>) -> Self {
        let is_tun = {
            let iface_rfc = stack.iface();
            let iface = iface_rfc.borrow();
            iface.device().is_tun()
        };

        let network = Self {
            name: Rc::new(name.to_string()),
            config,
            stack,
            is_tun,
            sender: Default::default(),
            poller: Default::default(),
            bindings: Default::default(),
            connections: Default::default(),
            handles: Default::default(),
            ingress: Default::default(),
            egress: Default::default(),
        };

        network.sender.net.borrow_mut().replace(network.clone());
        network.poller.net.borrow_mut().replace(network.clone());
        network
    }

    /// Returns a socket listening on an endpoint and ready for incoming
    /// connections. Sockets already connected won't be returned.
    pub fn get_bound(
        &self,
        protocol: Protocol,
        local_endpoint: impl Into<SocketEndpoint>,
    ) -> Option<SocketHandle> {
        let endpoint = local_endpoint.into();
        let iface_rfc = self.stack.iface();
        let iface = iface_rfc.borrow();
        let mut sockets = iface.sockets();
        sockets
            .find(|(handle, s)| {
                s.protocol() == protocol
                    && s.local_endpoint() == endpoint
                    // This condition prevents from returning socket connection instead
                    // of listening socket, unattached to connection.
                    && self.bindings.borrow().contains(handle)
            })
            .map(|(h, _)| h)
    }

    /// Listen on a local endpoint
    pub fn bind(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Result<SocketHandle> {
        let endpoint = endpoint.into();
        let handle = self.stack.bind(protocol, endpoint)?;
        self.bindings.borrow_mut().insert(handle);
        Ok(handle)
    }

    /// Stop listening on a local endpoint
    pub fn unbind(&self, protocol: Protocol, endpoint: impl Into<SocketEndpoint>) -> Result<()> {
        let endpoint = endpoint.into();
        let handle = self.stack.unbind(protocol, endpoint)?;
        self.bindings.borrow_mut().remove(&handle);
        Ok(())
    }

    /// Initiate a TCP connection
    pub fn connect(
        &self,
        remote: impl Into<IpEndpoint>,
        timeout: impl Into<Duration>,
    ) -> LocalBoxFuture<Result<Connection>> {
        let remote = remote.into();
        let timeout = timeout.into();

        let connect = match self.stack.connect(remote) {
            Ok(fut) => fut,
            Err(err) => return futures::future::err(err).boxed_local(),
        };
        self.poll();

        let connections = self.connections.clone();
        let handles = self.handles.clone();

        async move {
            let connection = match tokio::time::timeout(timeout, connect).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(error)) => return Err(error),
                _ => return Err(Error::ConnectionTimeout),
            };
            Self::add_connection_to(connection, &connections, &handles);
            Ok(connection)
        }
        .boxed_local()
    }

    /// Close all TCP connections with a remote IP address
    pub fn disconnect_all(
        &self,
        remote_ip: Box<[u8]>,
        timeout: impl Into<Duration>,
    ) -> LocalBoxFuture<()> {
        let (handles, futs): (Vec<_>, Vec<_>) = {
            let connections = self.connections.borrow();
            connections
                .values()
                .filter(|conn| {
                    conn.meta.remote.addr.as_bytes() == remote_ip.as_ref()
                        && conn.meta.protocol == Protocol::Tcp
                })
                .map(|conn| (conn.handle, self.stack.disconnect(conn.handle)))
                .unzip()
        };

        if futs.is_empty() {
            return futures::future::ready(()).boxed_local();
        }

        self.poll();

        let timeout = timeout.into();
        let net = self.clone();

        async move {
            let pending = futures::future::join_all(futs);
            let timeout = tokio::time::sleep(timeout).boxed_local();

            if let Either::Right((_, pending)) = futures::future::select(pending, timeout).await {
                handles.into_iter().for_each(|h| net.stack.abort(h));
                net.poll();

                let timeout = tokio::time::sleep(Duration::from_millis(500));
                let _ = futures::future::select(pending, timeout.boxed_local()).await;
            }
        }
        .boxed_local()
    }

    pub fn bindings(&self) -> core::cell::Ref<'_, HashSet<SocketHandle>> {
        self.bindings.borrow()
    }

    pub fn handles(&self) -> core::cell::Ref<'_, HashMap<SocketHandle, ConnectionMeta>> {
        self.handles.borrow()
    }

    pub fn connections(&self) -> core::cell::Ref<'_, HashMap<ConnectionMeta, Connection>> {
        self.connections.borrow()
    }

    pub fn sockets(&self) -> Vec<(SocketDesc, SocketState<ChannelMetrics>)> {
        let iface_rfc = self.stack.iface();
        let iface = iface_rfc.borrow();
        let metrics_rfc = self.stack.metrics();
        let metrics = metrics_rfc.borrow();

        iface
            .sockets()
            .map(|(_, s)| {
                let desc = s.desc();
                let metrics = metrics.get(&desc).cloned().unwrap_or_default();
                let mut state = s.state();
                state.set_inner(metrics);
                (desc, state)
            })
            .collect()
    }

    pub fn sockets_meta(&self) -> Vec<(SocketHandle, SocketDesc, SocketState<ChannelMetrics>)> {
        let iface_rfc = self.stack.iface();
        let iface = iface_rfc.borrow();
        let connections = self.handles.borrow();

        iface
            .sockets()
            .map(|(handle, s)| {
                (
                    handle,
                    connections
                        .get(&handle)
                        .cloned()
                        .map(|meta| meta.into())
                        .unwrap_or(s.desc()),
                    s.state(),
                )
            })
            .collect()
    }

    pub fn metrics(&self) -> ChannelMetrics {
        let iface_rfc = self.stack.iface();
        let iface = iface_rfc.borrow();
        iface.device().metrics()
    }

    #[inline(always)]
    fn is_connected(&self, meta: &ConnectionMeta) -> bool {
        self.connections.borrow().contains_key(meta)
    }

    #[inline(always)]
    fn add_connection(&self, connection: Connection) {
        Self::add_connection_to(connection, &self.connections, &self.handles);
    }

    fn add_connection_to(
        connection: Connection,
        connections: &Rc<RefCell<HashMap<ConnectionMeta, Connection>>>,
        handles: &Rc<RefCell<HashMap<SocketHandle, ConnectionMeta>>>,
    ) {
        let handle = connection.handle;
        let meta = connection.into();
        connections.borrow_mut().insert(meta, connection);
        handles.borrow_mut().insert(handle, meta);
    }

    #[inline(always)]
    fn remove_connection(&self, meta: &ConnectionMeta, handle: SocketHandle) {
        self.stack.remove(meta, handle);
        self.handles.borrow_mut().remove(&handle);
        self.sender.remove(&handle);

        if !meta.remote.is_specified() {
            return;
        }
        self.connections.borrow_mut().remove(meta);
    }

    /// Inject send data into the stack
    #[inline(always)]
    pub fn send<'a>(
        &self,
        data: impl Into<Payload>,
        connection: Connection,
    ) -> impl Future<Output = Result<()>> + 'a {
        self.sender.send(data.into(), connection)
    }

    /// Inject received data into the stack
    #[inline(always)]
    pub fn receive(&self, data: impl Into<Payload>) {
        self.stack.receive(data)
    }

    pub fn spawn_local(&self) {
        let interval = std::env::var(STACK_POLL_MS_ENV_VAR)
            .and_then(|s| s.parse::<u64>().map_err(|_| std::env::VarError::NotPresent))
            .and_then(|v| match v {
                0 => Err(std::env::VarError::NotPresent),
                v => Ok(v),
            })
            .unwrap_or(250);
        self.poller.clone().spawn(Duration::from_millis(interval));
    }

    /// Polls the inner network stack
    pub fn poll(&self) {
        loop {
            let finished = match (self.stack.poll(), self.is_tun) {
                (Ok(true), _) | (Ok(_), false) => self.process_ingress() && self.process_egress(),
                (Ok(false), _) => true,
                (Err(err), _) => {
                    log::warn!("{}: stack poll error: {}", *self.name, err);
                    false
                }
            };

            if finished {
                break;
            }
        }
    }

    /// Take the ingress traffic receive channel
    #[inline(always)]
    pub fn ingress_receiver(&self) -> Option<IngressReceiver> {
        self.ingress.receiver()
    }

    /// Take the egress traffic receive channel
    #[inline(always)]
    pub fn egress_receiver(&self) -> Option<EgressReceiver> {
        self.egress.receiver()
    }

    fn process_ingress(&self) -> bool {
        let mut finished = true;

        let iface_rfc = self.stack.iface();
        let mut iface = iface_rfc.borrow_mut();
        let mut bindings = self.bindings.borrow_mut();
        let mut events = Vec::new();
        let mut remove = Vec::new();
        let mut rebind = None;

        for (handle, socket) in iface.sockets_mut() {
            let mut desc = socket.desc();

            // When socket is closing, smoltcp clears remote endpoint at some point
            // to `Unspecified`. This is why SocketDesc and ConnectionMeta can differ here.
            // We will try to use ConnectionMeta, because it conveys more information.
            if socket.is_closed() {
                let meta = self
                    .handles
                    .borrow()
                    .get(&handle)
                    .copied()
                    .or(desc.try_into().ok());

                if let Some(meta) = meta {
                    // We had established connection with someone and it was closed.
                    log::debug!(
                        "{}: closing socket [{handle}]: {desc:?} / {meta:?}",
                        self.name
                    );
                    events.push(IngressEvent::Disconnected { desc: meta.into() });
                } else {
                    // Connection metadata was cleared.
                    // Socket probably got RST packet and it's state was cleared,
                    // or it was only listening socket that wasn't connected at any moment.
                    log::debug!("Removing socket {handle} with reset metadata");
                }

                remove.push((
                    meta.unwrap_or(ConnectionMeta::unspecified(desc.protocol)),
                    handle,
                ));
            }

            let mut received = 0;

            while socket.can_recv() {
                let (remote, payload) = match socket.recv() {
                    Ok(Some(tuple)) => tuple,
                    Ok(None) => break,
                    Err(err) => {
                        log::debug!("{}: ingress packet error: {err}", self.name);
                        continue;
                    }
                };

                let len = payload.len();

                received += len;
                desc.remote = remote.into();

                if let Ok(meta) = desc.try_into() {
                    if !self.is_connected(&meta) {
                        self.add_connection(Connection { handle, meta });
                        events.push(IngressEvent::InboundConnection { desc: meta.into() });
                    }
                }

                log::trace!("{}: ingress {len} B packet", self.name);

                self.stack.on_received(&desc, len);
                events.push(IngressEvent::Packet { desc, payload });

                if received >= self.config.max_recv_batch {
                    finished = false;
                    break;
                }
            }

            if bindings.contains(&handle) && socket.remote_endpoint().is_specified() {
                bindings.remove(&handle);
                rebind = Some((socket.protocol(), socket.local_endpoint()));

                finished = false;
                break;
            }
        }

        drop(bindings);
        drop(iface);

        remove.into_iter().for_each(|(meta, handle)| {
            self.remove_connection(&meta, handle);
        });

        if !events.is_empty() {
            let ingress_tx = self.ingress.tx.clone();
            for event in events {
                if ingress_tx.send(event).is_err() {
                    log::debug!(
                        "{}: ingress channel closed, unable to receive packets",
                        self.name
                    );
                    break;
                }
            }
        }

        if let Some((p, ep)) = rebind {
            if let Err(e) = self.bind(p, ep) {
                log::warn!("{}: cannot bind socket {p} {ep:?}: {e}", self.name);
            }
            let _ = self.stack.poll();
            return self.process_ingress();
        }

        finished
    }

    fn process_egress(&self) -> bool {
        let mut sent = 0;
        let mut finished = true;

        let iface_rfc = self.stack.iface();
        let mut iface = iface_rfc.borrow_mut();
        let device = iface.device_mut();
        let is_tun = device.is_tun();

        while let Some(data) = device.next_phy_tx() {
            match {
                if is_tun {
                    EgressEvent::from_ip_packet(data)
                } else {
                    EgressEvent::from_eth_frame(data)
                }
            } {
                Ok(event) => {
                    sent += event.payload.len();

                    if let Some((desc, size)) = event.desc.as_ref() {
                        self.stack.on_sent(desc, *size);
                    }

                    if self.egress.tx.send(event).is_err() {
                        log::trace!(
                            "{}: egress channel closed, unable to send packets",
                            *self.name
                        );
                        break;
                    }
                }
                Err(err) => log::trace!("{}: egress packet error: {}", *self.name, err),
            }

            if sent >= self.config.max_send_batch {
                finished = false;
                continue;
            }
        }

        finished
    }
}

#[derive(Clone, Debug)]
pub enum IngressEvent {
    /// New connection to a bound endpoint
    InboundConnection { desc: SocketDesc },
    /// Disconnection from a bound endpoint
    Disconnected { desc: SocketDesc },
    /// Bound endpoint packet
    Packet { desc: SocketDesc, payload: Vec<u8> },
}

#[derive(Clone, Debug)]
pub struct EgressEvent {
    pub remote: Box<[u8]>,
    pub payload: Box<[u8]>,
    pub desc: Option<(SocketDesc, usize)>,
}

impl EgressEvent {
    pub fn from_eth_frame(data: Vec<u8>) -> Result<Self> {
        let frame = EtherFrame::try_from(data)?;
        let (desc, remote) = match &frame {
            EtherFrame::Ip(_) => {
                let data = frame.payload();
                IpPacket::peek(data)?;

                let packet = IpPacket::packet(data);
                let remote = packet.dst_address().into();
                let desc = Self::payload_desc(&packet);
                (desc, remote)
            }
            EtherFrame::Arp(_) => {
                let packet = ArpPacket::packet(frame.payload());
                let remote = packet.get_field(ArpField::TPA).into();
                (None, remote)
            }
        };

        Ok(EgressEvent {
            remote,
            payload: frame.into(),
            desc,
        })
    }

    pub fn from_ip_packet(data: Vec<u8>) -> Result<Self> {
        let (desc, remote) = {
            IpPacket::peek(&data)?;
            let packet = IpPacket::packet(&data);
            let remote = packet.dst_address().into();
            let desc = Self::payload_desc(&packet);

            (desc, remote)
        };

        Ok(EgressEvent {
            remote,
            payload: data.into_boxed_slice(),
            desc,
        })
    }

    fn payload_desc(packet: &IpPacket) -> Option<(SocketDesc, usize)> {
        let protocol = Protocol::try_from(packet.protocol()).ok()?;

        let (local_port, remote_port, size) = match protocol {
            Protocol::Tcp => {
                TcpPacket::peek(packet.payload()).ok()?;
                let tcp = TcpPacket::packet(packet.payload());
                (tcp.src_port(), tcp.dst_port(), tcp.payload_size)
            }
            Protocol::Udp => {
                UdpPacket::peek(packet.payload()).ok()?;
                let udp = UdpPacket::packet(packet.payload());
                (udp.src_port(), udp.dst_port(), udp.payload_size)
            }
            _ => return None,
        };

        let local_ip = ip_ntoh(packet.src_address())?;
        let remote_ip = ip_ntoh(packet.dst_address())?;

        let desc = SocketDesc {
            protocol,
            local: (local_ip, local_port).into(),
            remote: (remote_ip, remote_port).into(),
        };

        Some((desc, size))
    }
}

#[derive(Clone, Default)]
struct StackSender {
    inner: Rc<RefCell<StackSenderInner>>,
    net: Rc<RefCell<Option<Network>>>,
}

impl StackSender {
    #[inline]
    pub fn send<'a>(
        &self,
        data: Payload,
        conn: Connection,
    ) -> impl Future<Output = Result<()>> + 'a {
        let mut sender = {
            match {
                let inner = self.inner.borrow();
                inner.map.get(&conn.handle).cloned()
            } {
                Some(sender) => sender,
                None => self.spawn(conn.handle),
            }
        };

        async move { sender.send((data, conn)).map_err(Error::from).await }
    }

    fn spawn(&self, handle: SocketHandle) -> mpsc::Sender<(Payload, Connection)> {
        let net = self.net.borrow().clone().expect("Network not initialized");
        let (tx, rx) = mpsc::channel(1);

        spawn_local(async move {
            rx.for_each(|(vec, conn)| {
                let net = net.clone();
                let stack = net.stack.clone();
                async move {
                    let _ = stack.send(vec, conn, move || net.poll()).await;
                }
            })
            .await;
        });

        let mut inner = self.inner.borrow_mut();
        inner.map.insert(handle, tx.clone());

        tx
    }

    pub fn remove(&self, handle: &SocketHandle) {
        let mut inner = self.inner.borrow_mut();
        if let Some(mut tx) = inner.map.remove(handle) {
            spawn_local(async move {
                let _ = tx.close().await;
            });
        }
    }
}

#[derive(Default)]
struct StackSenderInner {
    map: HashMap<SocketHandle, mpsc::Sender<(Payload, Connection)>>,
}

#[derive(Clone, Default)]
struct StackPoller {
    net: Rc<RefCell<Option<Network>>>,
}

impl StackPoller {
    pub fn spawn(&self, interval: Duration) {
        let poller = self.clone();
        spawn_local(async move {
            let mut interval = tokio::time::interval(interval);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                poller.net.borrow().as_ref().unwrap().poll();
            }
        });
    }
}

#[derive(Clone)]
pub struct Channel<T> {
    pub tx: UnboundedSender<T>,
    rx: Rc<RefCell<Option<UnboundedReceiver<T>>>>,
}

impl<T> Channel<T> {
    pub fn receiver(&self) -> Option<UnboundedReceiver<T>> {
        self.rx.borrow_mut().take()
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            tx,
            rx: Rc::new(RefCell::new(Some(rx))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::rc::Rc;
    use std::time::Duration;

    use futures::channel::{mpsc, oneshot};
    use futures::{Sink, SinkExt, Stream, StreamExt};
    use sha3::Digest;
    use tokio::task::spawn_local;
    use tokio_stream::wrappers::UnboundedReceiverStream;
    use ya_smoltcp::iface::Route;
    use ya_smoltcp::phy::Medium;
    use ya_smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};

    use crate::interface::{add_iface_address, add_iface_route, ip_to_mac, tap_iface, tun_iface};
    use crate::{Connection, EgressEvent, IngressEvent, Network, Protocol, Stack, StackConfig};

    const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

    fn new_network(medium: Medium, ip: IpAddress, config: StackConfig) -> Network {
        let config = Rc::new(config);
        let cidr = IpCidr::new(ip, 16);
        let route = match ip {
            IpAddress::Ipv4(ipv4) => Route::new_ipv4_gateway(ipv4),
            IpAddress::Ipv6(ipv6) => Route::new_ipv6_gateway(ipv6),
            _ => panic!("unspecified address"),
        };

        let mut iface = match medium {
            Medium::Ethernet => tap_iface(ip_to_mac(ip), config.max_transmission_unit),
            Medium::Ip => tun_iface(config.max_transmission_unit),
            _ => panic!("unsupported medium: {:?}", medium),
        };

        add_iface_address(&mut iface, cidr);
        add_iface_route(&mut iface, cidr, route);
        Network::new(
            format!("[{:?}] {}", medium, ip),
            config.clone(),
            Stack::new(iface, config),
        )
    }

    fn produce_data<S, E>(
        mut tx: S,
        total: usize,
        chunk_size: usize,
    ) -> oneshot::Receiver<anyhow::Result<(S, Vec<u8>)>>
    where
        S: Sink<Vec<u8>, Error = E> + Unpin + 'static,
        E: Into<anyhow::Error>,
    {
        let (dtx, drx) = oneshot::channel();

        spawn_local(async move {
            let mut digest = sha3::Sha3_224::new();
            let mut sent = 0;
            let mut err = None;

            while sent < total {
                let vec: Vec<u8> = (0..chunk_size.min(total - sent))
                    .map(|_| rand::random())
                    .collect();

                digest.input(&vec);
                sent += vec.len();

                if let Err(e) = tx.send(vec).await {
                    err = Some(e);
                    break;
                }
            }

            println!("Produced {} B", sent);
            match err {
                Some(e) => dtx.send(Err(e.into())),
                None => dtx.send(Ok((tx, digest.result().to_vec()))),
            }
        });

        drx
    }

    fn consume_data(
        mut rx: mpsc::Receiver<Vec<u8>>,
        total: usize,
    ) -> oneshot::Receiver<anyhow::Result<Vec<u8>>> {
        let (dtx, drx) = oneshot::channel();

        spawn_local(async move {
            let mut digest = sha3::Sha3_224::new();
            let mut read: usize = 0;

            while let Some(vec) = rx.next().await {
                let len = vec.len();

                read += len;
                digest.input(&vec);

                if read >= total {
                    break;
                }
            }

            println!("Consumed {} B", read);
            let _ = dtx.send(Ok(digest.result().to_vec()));
        });

        drx
    }

    fn net_inject<S>(rx: S, net: Network)
    where
        S: Stream<Item = Vec<u8>> + 'static,
    {
        spawn_local(async move {
            rx.for_each(|vec| {
                let net = net.clone();
                async move {
                    net.receive(vec);
                    net.poll();
                }
            })
            .await;
        });
    }

    fn net_inject2<S>(rx: S, net1: Network, net2: Network)
    where
        S: Stream<Item = EgressEvent> + 'static,
    {
        let ip1 = net1
            .stack
            .address()
            .unwrap()
            .address()
            .as_bytes()
            .to_vec()
            .into_boxed_slice();

        spawn_local(async move {
            rx.for_each(|event| {
                let net = if event.remote == ip1 {
                    net1.clone()
                } else {
                    net2.clone()
                };
                async move {
                    net.receive(event.payload);
                    net.poll();
                }
            })
            .await;
        });
    }

    fn net_send<S>(rx: S, net: Network, conn: Connection)
    where
        S: Stream<Item = Vec<u8>> + 'static,
    {
        spawn_local(async move {
            let net = net.clone();
            rx.for_each(|vec| async {
                let _ = net
                    .send(vec, conn)
                    .await
                    .map_err(|e| eprintln!("failed to send packet: {}", e));
            })
            .await;
        });
    }

    fn net_receive<Si, St, E>(tx: Si, rx: St)
    where
        Si: Sink<Vec<u8>, Error = E> + Clone + Unpin + 'static,
        St: Stream<Item = IngressEvent> + 'static,
        E: Into<anyhow::Error> + Debug,
    {
        spawn_local(async move {
            rx.for_each(move |event| {
                let mut tx = tx.clone();
                async move {
                    match event {
                        IngressEvent::Packet { payload, .. } => {
                            if let Err(e) = tx.send(payload).await {
                                eprintln!("net send error: {:?}", e);
                            }
                        }
                        IngressEvent::Disconnected { desc } => {
                            println!("disconnected: {:?}", desc);
                        }
                        IngressEvent::InboundConnection { desc } => {
                            println!("inbound connection: {:?}", desc);
                        }
                    }
                }
            })
            .await;
        });
    }

    /// Generate, send and receive data across 2 network instances
    async fn net_exchange(medium: Medium, total: usize, chunk_size: usize) -> anyhow::Result<()> {
        const MTU: usize = 65535;

        println!(">> exchanging {} B in {} B chunks", total, chunk_size);

        let ip1 = Ipv4Address::new(10, 0, 0, 1);
        let ip2 = Ipv4Address::new(10, 0, 0, 2);

        let config = StackConfig {
            max_transmission_unit: MTU,
            ..Default::default()
        };

        let net1 = new_network(medium, ip1.into(), config.clone());
        let net2 = new_network(medium, ip2.into(), config.clone());

        net1.spawn_local();
        net2.spawn_local();

        net1.bind(Protocol::Tcp, (ip1, 1))?;
        net2.bind(Protocol::Tcp, (ip2, 1))?;

        // net 1
        // inject egress packets from net 2 into net 1 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net2.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net1.clone(),
        );
        // process net 1 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net1.ingress_receiver().unwrap()),
        );
        // future for calculating checksum from data received by net 1
        let consume1 = consume_data(rx, total);

        // net 2
        // inject egress packets from net 1 into net 2 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net1.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net2.clone(),
        );
        // process net 2 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net2.ingress_receiver().unwrap()),
        );
        // future for calculating checksum from data received by net 2
        let consume2 = consume_data(rx, total);

        let conn1 = net1.connect((ip2, 1), Duration::from_secs(3)).await?;
        let conn2 = net2.connect((ip1, 1), Duration::from_secs(3)).await?;

        net1.poll();
        net2.poll();

        let (tx, rx) = mpsc::channel(1);
        let produce1 = produce_data(tx, total, chunk_size);
        net_send(rx, net1.clone(), conn1);

        let (tx, rx) = mpsc::channel(1);
        let produce2 = produce_data(tx, total, chunk_size);
        net_send(rx, net2.clone(), conn2);

        let (f1, f2, f3, f4) = futures::future::join4(produce1, produce2, consume1, consume2).await;

        let (mut ptx1, produced1) = f1??;
        let (mut ptx2, produced2) = f2??;
        let consumed1 = f3??;
        let consumed2 = f4??;

        let _ = ptx1.close().await;
        let _ = ptx2.close().await;

        assert_eq!(hex::encode(produced1), hex::encode(consumed2));
        assert_eq!(hex::encode(produced2), hex::encode(consumed1));

        Ok(())
    }

    async fn re_bind(medium: Medium, total: usize, chunk_size: usize) -> anyhow::Result<()> {
        const MTU: usize = 65535;

        println!(">> exchanging {} B in {} B chunks", total, chunk_size);

        let ip1 = Ipv4Address::new(10, 0, 0, 1);
        let ip2 = Ipv4Address::new(10, 0, 0, 2);
        let ip3 = Ipv4Address::new(10, 0, 0, 3);

        let config = StackConfig {
            max_transmission_unit: MTU,
            ..Default::default()
        };

        let net1 = new_network(medium, ip1.into(), config.clone());
        let net2 = new_network(medium, ip2.into(), config.clone());
        let net3 = new_network(medium, ip3.into(), config.clone());

        net1.spawn_local();
        net2.spawn_local();
        net3.spawn_local();

        net1.bind(Protocol::Tcp, (ip1, 1))?;
        net2.bind(Protocol::Tcp, (ip2, 1))?;
        net3.bind(Protocol::Tcp, (ip3, 1))?;

        // net 1
        // inject egress packets from net 2 into net 1 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net2.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net1.clone(),
        );
        // inject egress packets from net 3 into net 1 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net3.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net1.clone(),
        );
        // process net 1 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net1.ingress_receiver().unwrap()),
        );

        let _consume1 = spawn_local(rx.for_each(|e| async move { println!("consumer 1: {e:?}") }));

        // net 2
        // inject egress packets from net 1 into net 2 or net 3 rx buffer
        net_inject2(
            UnboundedReceiverStream::new(net1.egress_receiver().unwrap()),
            net2.clone(),
            net3.clone(),
        );

        // process net 2 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net2.ingress_receiver().unwrap()),
        );

        let _consume2 = spawn_local(rx.for_each(|e| async move { println!("consumer 2: {e:?}") }));

        // process net 3 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net3.ingress_receiver().unwrap()),
        );

        let _consume3 = spawn_local(rx.for_each(|e| async move { println!("consumer 3: {e:?}") }));

        let conn1 = net2.connect((ip1, 1), Duration::from_secs(3));
        let conn2 = net3.connect((ip1, 1), Duration::from_secs(3));

        let (f1, f2) = futures::future::join(conn1, conn2).await;

        f1.expect("Connection failed!");
        f2.expect("Connection failed!");

        Ok(())
    }

    /// Establish given number of connections between single client and server
    #[cfg(feature = "test-suite")]
    async fn establish_multiple_conn(
        medium: Medium,
        total: usize,
        chunk_size: usize,
        conn_num: u16,
    ) -> anyhow::Result<()> {
        use crate::error;

        const MTU: usize = 65535;

        println!(">> exchanging {} B in {} B chunks", total, chunk_size);

        let ip1 = Ipv4Address::new(10, 0, 0, 1);
        let ip2 = Ipv4Address::new(10, 0, 0, 2);

        let config = StackConfig {
            max_transmission_unit: MTU,
            ..Default::default()
        };

        let net1 = new_network(medium, ip1.into(), config.clone());
        let net2 = new_network(medium, ip2.into(), config.clone());

        net1.spawn_local();
        net2.spawn_local();

        net1.bind(Protocol::Tcp, (ip1, 1))?;
        net2.bind(Protocol::Tcp, (ip2, 1))?;

        // net 1
        // inject egress packets from net 2 into net 1 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net2.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net1.clone(),
        );

        // process net 1 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net1.ingress_receiver().unwrap()),
        );

        let _consume1 = spawn_local(rx.for_each(|e| async move { println!("consumer 1: {e:?}") }));

        // net 2
        // inject egress packets from net 1 into net 2 rx buffer
        net_inject(
            UnboundedReceiverStream::new(net1.egress_receiver().unwrap())
                .map(|e| e.payload.into_vec()),
            net2.clone(),
        );

        // process net 2 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(
            tx,
            UnboundedReceiverStream::new(net2.ingress_receiver().unwrap()),
        );

        let _consume2 = spawn_local(rx.for_each(|e| async move { println!("consumer 2: {e:?}") }));

        for i in 1..=conn_num {
            let conn = net2.connect((ip1, 1), Duration::from_secs(3)).await;
            match conn {
                Ok(_) => println!("Connection({i}) successful"),
                Err(e) => {
                    if i != u16::MAX {
                        panic!("Connection failed! Error: {}", e.to_string());
                    };

                    let expected = error::Error::Other("no ports available".into());
                    assert_eq!(expected, e)
                }
            }
        }

        Ok(())
    }

    async fn spawn_exchange(medium: Medium, total: usize, chunk_size: usize) -> anyhow::Result<()> {
        tokio::task::LocalSet::new()
            .run_until(tokio::time::timeout(
                EXCHANGE_TIMEOUT,
                net_exchange(medium, total, chunk_size),
            ))
            .await?
    }

    async fn spawn_exchange_scenarios(medium: Medium) -> anyhow::Result<()> {
        spawn_exchange(medium, 1024, 1).await?;
        spawn_exchange(medium, 1024, 4).await?;
        spawn_exchange(medium, 1024, 7).await?;
        spawn_exchange(medium, 10240, 16).await?;
        spawn_exchange(medium, 1024000, 383).await?;
        spawn_exchange(medium, 1024000, 384).await?;
        spawn_exchange(medium, 1024000, 4096).await?;
        spawn_exchange(medium, 1024000, 40960).await?;

        #[cfg(not(debug_assertions))]
        {
            spawn_exchange(medium, 10240000, 40960).await?;
            spawn_exchange(medium, 10240000, 131070).await?;
            spawn_exchange(medium, 10240000, 1024000).await?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn tap_exchange() -> anyhow::Result<()> {
        spawn_exchange_scenarios(Medium::Ethernet).await
    }

    #[tokio::test]
    async fn tun_exchange() -> anyhow::Result<()> {
        spawn_exchange_scenarios(Medium::Ip).await
    }

    #[tokio::test]
    async fn socket_re_binding() -> anyhow::Result<()> {
        tokio::task::LocalSet::new()
            .run_until(tokio::time::timeout(
                EXCHANGE_TIMEOUT,
                re_bind(Medium::Ip, 0, 0),
            ))
            .await?
    }

    // Test case where establishing a maximum number of connections (equal to 65 534 connections) does not fail.
    #[cfg(feature = "test-suite")]
    #[tokio::test]
    async fn multiple_conn() -> anyhow::Result<()> {
        tokio::task::LocalSet::new()
            .run_until(establish_multiple_conn(Medium::Ip, 0, 0, u16::MAX - 1))
            .await
    }

    // Test case where establishing a new connection (above the number of 65 534 connections) results in the "no ports available" error.
    #[cfg(feature = "test-suite")]
    #[tokio::test]
    async fn overload_conn() -> anyhow::Result<()> {
        tokio::task::LocalSet::new()
            .run_until(establish_multiple_conn(Medium::Ip, 0, 0, u16::MAX))
            .await
    }
}
