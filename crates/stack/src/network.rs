use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ops::Mul;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::{Either, LocalBoxFuture};
use futures::{Future, FutureExt, StreamExt};
use smoltcp::iface::SocketHandle;
use smoltcp::wire::IpEndpoint;
use tokio::sync::mpsc;
use tokio::task::{spawn_local, JoinHandle};

use crate::connection::{Connection, ConnectionMeta};
use crate::packet::{
    ip_ntoh, ArpField, ArpPacket, EtherFrame, IpPacket, PeekPacket, TcpPacket, UdpPacket,
};
use crate::protocol::Protocol;
use crate::queue::{ProcessedFuture, Queue};
use crate::socket::{SocketDesc, SocketEndpoint, SocketExt, SocketState};
use crate::stack::Stack;
use crate::{ChannelMetrics, Error, Result};

pub type IngressReceiver = mpsc::UnboundedReceiver<IngressEvent>;
pub type EgressReceiver = mpsc::UnboundedReceiver<EgressEvent>;

#[derive(Clone)]
pub struct NetworkConfig {
    pub max_transmission_unit: usize,
    /// Size of TCP send and receive buffer as multiply of MTU.
    pub buffer_size_multiplier: usize,
}

#[derive(Clone)]
pub struct Network {
    pub name: Rc<String>,
    pub config: Rc<NetworkConfig>,
    pub stack: Stack<'static>,
    sender: StackSender,
    poller: StackPoller,
    bindings: Rc<RefCell<HashSet<(Protocol, SocketEndpoint)>>>,
    connections: Rc<RefCell<HashMap<ConnectionMeta, Connection>>>,
    handles: Rc<RefCell<HashMap<SocketHandle, ConnectionMeta>>>,
    ingress: Channel<IngressEvent>,
    egress: Channel<EgressEvent>,
}

impl Network {
    /// Creates a new Network instance
    pub fn new(name: impl ToString, config: Rc<NetworkConfig>, stack: Stack<'static>) -> Self {
        let network = Self {
            name: Rc::new(name.to_string()),
            config,
            stack,
            sender: Default::default(),
            poller: Default::default(),
            bindings: Default::default(),
            connections: Default::default(),
            handles: Default::default(),
            ingress: Default::default(),
            egress: Default::default(),
        };

        network.poller.net.borrow_mut().replace(network.clone());
        network
    }

    /// Returns a socket bound on an endpoint
    pub fn get_bound(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Option<SocketHandle> {
        let endpoint = endpoint.into();
        let iface_rfc = self.stack.iface();
        let iface = iface_rfc.borrow();
        let mut sockets = iface.sockets();
        sockets
            .find(|(_, s)| s.protocol() == protocol && s.local_endpoint() == endpoint)
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
        self.bindings.borrow_mut().insert((protocol, endpoint));
        Ok(handle)
    }

    /// Stop listening on a local endpoint
    pub fn unbind(&self, protocol: Protocol, endpoint: impl Into<SocketEndpoint>) -> Result<()> {
        let endpoint = endpoint.into();
        self.stack.unbind(protocol, endpoint)?;
        self.bindings.borrow_mut().remove(&(protocol, endpoint));
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
            let timeout = tokio::time::delay_for(timeout);

            if let Either::Right((_, pending)) = futures::future::select(pending, timeout).await {
                handles.into_iter().for_each(|h| net.stack.abort(h));
                net.poll();

                let timeout = tokio::time::delay_for(Duration::from_millis(500));
                let _ = futures::future::select(pending, timeout).await;
            }
        }
        .boxed_local()
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
        if !meta.remote.is_specified() {
            return;
        }
        self.stack.remove(meta, handle);
        self.connections.borrow_mut().remove(meta);
    }

    /// Inject send data into the stack
    #[inline(always)]
    pub fn send<'a>(
        &self,
        data: impl Into<Vec<u8>>,
        connection: Connection,
    ) -> Result<ProcessedFuture<'a>> {
        self.sender.send(data.into(), connection)
    }

    /// Inject received data into the stack
    #[inline(always)]
    pub fn receive(&self, data: impl Into<Vec<u8>>) {
        self.stack.receive(data)
    }

    pub fn spawn_local(&self) {
        let net = self.clone();
        let stack = self.stack.clone();

        self.poller.clone().spawn();
        self.sender.spawn_local(move |vec, conn| {
            let net = net.clone();
            let stack = stack.clone();

            async move { stack.send(vec, conn, move || net.poll()).await }
        });
    }

    /// Polls the inner network stack
    pub fn poll(&self) {
        let mut sent = 0;
        let mut received = 0;

        loop {
            if let Err(err) = self.stack.poll() {
                log::warn!("{}: stack poll error: {}", *self.name, err);
            }

            let (ingress, loop_received) = self.process_ingress();
            let (egress, loop_sent) = self.process_egress();

            sent += loop_sent;
            received += loop_received;

            if !egress && !ingress {
                break;
            }
        }

        self.poller.adjust_strategy(sent, received);
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

    fn process_ingress(&self) -> (bool, usize) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let mut processed = false;
        let mut received = 0;

        let iface_rfc = self.stack.iface();
        let mut iface = iface_rfc.borrow_mut();
        let mut events = Vec::new();
        let mut remove = Vec::new();
        let mut rebind = self.bindings.borrow().clone();

        for (handle, socket) in iface.sockets_mut() {
            let mut desc = SocketDesc {
                protocol: socket.protocol(),
                local: socket.local_endpoint(),
                remote: socket.remote_endpoint(),
            };

            if socket.is_closed() {
                match { self.handles.borrow().get(&handle).cloned() } {
                    Some(meta) => {
                        remove.push((meta, handle));
                        if let Ok(desc) = meta.try_into() {
                            events.push(IngressEvent::Disconnected { desc })
                        }
                    }
                    None => {
                        if !desc.local.is_specified() {
                            // skip; the socket is initializing
                            continue;
                        }
                        if let Ok(meta) = desc.try_into() {
                            log::trace!("{}: removing unregistered socket {:?}", self.name, desc);
                            remove.push((meta, handle));
                            events.push(IngressEvent::Disconnected { desc });
                        }
                    }
                };
                continue;
            }

            if !desc.remote.is_specified() {
                rebind.remove(&(desc.protocol, desc.local));
            }

            while socket.can_recv() {
                let (remote, payload) = match socket.recv() {
                    Ok(Some(tuple)) => {
                        processed = true;
                        tuple
                    }
                    Ok(None) => break,
                    Err(err) => {
                        log::trace!("{}: ingress packet error: {}", self.name, err);
                        processed = true;
                        continue;
                    }
                };

                desc.remote = remote.into();
                if let Ok(meta) = desc.try_into() {
                    if !self.is_connected(&meta) {
                        self.add_connection(Connection { handle, meta });
                        events.push(IngressEvent::InboundConnection { desc });
                    }
                }

                log::trace!("{}: ingress {} B IP packet", self.name, payload.len());

                received += 1;
                self.stack.on_received(&desc, payload.len());

                let no = COUNTER.fetch_add(1, Ordering::Relaxed);
                events.push(IngressEvent::Packet { desc, payload, no });
            }
        }

        drop(iface);
        remove.into_iter().for_each(|(meta, handle)| {
            self.remove_connection(&meta, handle);
        });

        for (p, ep) in rebind {
            if let Err(e) = self.bind(p, ep) {
                log::trace!("{}: ingress: cannot bind {} {:?}: {}", self.name, p, ep, e);
            }
        }

        if !events.is_empty() {
            let ingress_tx = self.ingress.tx.clone();
            for event in events {
                if ingress_tx.send(event).is_err() {
                    log::trace!(
                        "{}: ingress channel closed, unable to receive packets",
                        self.name
                    );
                    break;
                }
            }
        }

        (processed, received)
    }

    fn process_egress(&self) -> (bool, usize) {
        let mut processed = false;
        let mut sent = 0;

        let iface_rfc = self.stack.iface();
        let mut iface = iface_rfc.borrow_mut();
        let device = iface.device_mut();
        let is_tun = device.is_tun();

        while let Some(data) = device.next_phy_tx() {
            processed = true;

            match {
                if is_tun {
                    EgressEvent::from_ip_packet(data)
                } else {
                    EgressEvent::from_eth_frame(data)
                }
            } {
                Ok(event) => {
                    sent += 1;

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
        }

        (processed, sent)
    }
}

#[derive(Clone, Debug)]
pub enum IngressEvent {
    /// New connection to a bound endpoint
    InboundConnection { desc: SocketDesc },
    /// Disconnection from a bound endpoint
    Disconnected { desc: SocketDesc },
    /// Bound endpoint packet
    Packet {
        desc: SocketDesc,
        payload: Vec<u8>,
        no: usize,
    },
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
}

impl StackSender {
    pub fn send<'a>(&self, data: Vec<u8>, conn: Connection) -> Result<ProcessedFuture<'a>> {
        let mut sender = {
            let inner = self.inner.borrow();
            inner.queue.sender()
        };
        sender.send((data, conn))
    }

    pub fn spawn_local<H, F>(&self, mut handler: H)
    where
        H: FnMut(Vec<u8>, Connection) -> F + 'static,
        F: Future<Output = Result<()>> + 'static,
    {
        let mut inner = self.inner.borrow_mut();

        if let Some(rx) = inner.queue.receiver() {
            let handle = spawn_local(async move {
                rx.for_each(|((vec, conn), tx)| {
                    let fut = handler(vec, conn);
                    async move {
                        let _ = tx.send(fut.await);
                    }
                })
                .await;
            });
            inner.handle.replace(handle);
        }
    }
}

#[derive(Default)]
struct StackSenderInner {
    queue: Queue<(Vec<u8>, Connection)>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
struct StackPoller {
    inner: Rc<RefCell<StackPollerInner>>,
    net: Rc<RefCell<Option<Network>>>,
}

impl StackPoller {
    pub fn spawn(&self) {
        if self.inner.borrow().handle.is_none() {
            let poller = self.clone();
            let handle = spawn_local(async move {
                loop {
                    let delay = poller.interval();
                    tokio::time::delay_for(delay).await;
                    poller.net.borrow().as_ref().unwrap().poll();
                }
            });
            self.inner.borrow_mut().handle.replace(handle);
        }
    }

    fn interval(&self) -> Duration {
        self.inner.borrow().strategy.interval()
    }

    fn adjust_strategy(&self, sent: usize, received: usize) {
        self.inner.borrow_mut().strategy.adjust(sent, received)
    }
}

#[derive(Default)]
struct StackPollerInner {
    handle: Option<JoinHandle<()>>,
    strategy: StackPollerStrategy,
}

#[derive(Clone, Copy, Debug)]
enum StackPollerStrategy {
    Default {
        at: Instant,
        current: Duration,
    },
    Adaptive {
        at: Instant,
        current: Duration,
        factor: u8,
    },
}

impl Default for StackPollerStrategy {
    fn default() -> Self {
        Self::Default {
            at: Instant::now(),
            current: Self::DEFAULT_INTERVAL,
        }
    }
}

impl StackPollerStrategy {
    const MIN_INTERVAL: Duration = Duration::from_micros(1);
    const DEFAULT_INTERVAL: Duration = Duration::from_millis(750);
    const DEFAULT_FACTOR: u8 = 2;

    fn at(&self) -> Instant {
        match self {
            Self::Default { at, .. } | Self::Adaptive { at, .. } => *at,
        }
    }

    fn interval(&self) -> Duration {
        match self {
            Self::Default { current, .. } | Self::Adaptive { current, .. } => *current,
        }
    }

    fn adjust(&mut self, sent: usize, received: usize) {
        if sent > 0 || received > 0 {
            self.speed_up();
        } else {
            self.slow_down();
        }
    }

    fn speed_up(&mut self) {
        let now = Instant::now();
        let latent = self.at() + Self::DEFAULT_INTERVAL >= now;

        match self {
            Self::Default { current, .. } | Self::Adaptive { current, .. } => {
                let now = Instant::now();
                let factor = Self::DEFAULT_FACTOR;

                let next = if latent {
                    Self::MIN_INTERVAL
                } else {
                    Self::MIN_INTERVAL.max(current.div_f32(factor as f32))
                };

                *self = Self::Adaptive {
                    at: now,
                    current: next,
                    factor,
                }
            }
        };
    }

    fn slow_down(&mut self) {
        match self {
            Self::Default { .. } => {}
            Self::Adaptive {
                current, factor, ..
            } => {
                let now = Instant::now();
                let next = Self::DEFAULT_INTERVAL.min(current.mul(*factor as u32));

                if next == Self::DEFAULT_INTERVAL {
                    *self = Self::Default {
                        at: now,
                        current: next,
                    }
                } else {
                    *self = Self::Adaptive {
                        at: now,
                        current: next,
                        factor: *factor,
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Channel<T> {
    pub tx: mpsc::UnboundedSender<T>,
    rx: Rc<RefCell<Option<mpsc::UnboundedReceiver<T>>>>,
}

impl<T> Channel<T> {
    pub fn receiver(&self) -> Option<mpsc::UnboundedReceiver<T>> {
        self.rx.borrow_mut().take()
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
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
    use smoltcp::iface::Route;
    use smoltcp::phy::Medium;
    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
    use tokio::task::spawn_local;

    use crate::interface::{add_iface_address, add_iface_route, ip_to_mac, tap_iface, tun_iface};
    use crate::{Connection, IngressEvent, Network, NetworkConfig, Protocol, Stack};

    const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

    fn new_network(medium: Medium, ip: IpAddress, config: NetworkConfig) -> Network {
        let config = Rc::new(config);
        let cidr = IpCidr::new(ip.into(), 16);
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

    fn net_send<S>(rx: S, net: Network, conn: Connection)
    where
        S: Stream<Item = Vec<u8>> + 'static,
    {
        spawn_local(async move {
            let net = net.clone();
            rx.for_each(|vec| async {
                let _ = net
                    .send(vec, conn)
                    .unwrap_or_else(|e| Box::pin(futures::future::err(e)))
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
        const BUF_MULTIPLIER: usize = 32;

        println!(">> exchanging {} B in {} B chunks", total, chunk_size);

        let ip1 = Ipv4Address::new(10, 0, 0, 1);
        let ip2 = Ipv4Address::new(10, 0, 0, 2);

        let config = NetworkConfig {
            max_transmission_unit: MTU,
            buffer_size_multiplier: BUF_MULTIPLIER,
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
            net2.egress_receiver()
                .unwrap()
                .map(|e| e.payload.into_vec()),
            net1.clone(),
        );
        // process net 1 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(tx, net1.ingress_receiver().unwrap());
        // future for calculating checksum from data received by net 1
        let consume1 = consume_data(rx, total);

        // net 2
        // inject egress packets from net 1 into net 2 rx buffer
        net_inject(
            net1.egress_receiver()
                .unwrap()
                .map(|e| e.payload.into_vec()),
            net2.clone(),
        );
        // process net 2 events
        let (tx, rx) = mpsc::channel(1);
        net_receive(tx, net2.ingress_receiver().unwrap());
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
}
