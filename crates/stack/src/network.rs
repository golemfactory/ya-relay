use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ops::{DerefMut, Mul};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use futures::{Future, FutureExt, StreamExt};
use smoltcp::socket::{Socket, SocketHandle};
use smoltcp::wire::IpEndpoint;
use tokio::sync::mpsc;
use tokio::task::{spawn_local, JoinHandle};

use crate::connection::{Connection, ConnectionMeta};
use crate::packet::{ArpField, ArpPacket, EtherFrame, IpPacket, PeekPacket};
use crate::protocol::Protocol;
use crate::queue::{ProcessedFuture, Queue};
use crate::socket::{SocketDesc, SocketEndpoint, SocketExt};
use crate::stack::Stack;
use crate::{Error, Result};

pub type IngressReceiver = mpsc::UnboundedReceiver<IngressEvent>;
pub type EgressReceiver = mpsc::UnboundedReceiver<EgressEvent>;

const MAX_TCP_PAYLOAD_SIZE: usize = 64000;

#[derive(Clone)]
pub struct Network {
    pub name: Rc<String>,
    pub stack: Stack<'static>,
    sender: StackSender,
    poller: StackPoller,
    bindings: Rc<RefCell<HashSet<SocketHandle>>>,
    connections: Rc<RefCell<HashMap<ConnectionMeta, Connection>>>,
    ingress: Channel<IngressEvent>,
    egress: Channel<EgressEvent>,
}

impl Network {
    /// Creates a new Network instance
    pub fn new(name: impl ToString, stack: Stack<'static>) -> Self {
        let network = Self {
            name: Rc::new(name.to_string()),
            stack,
            sender: Default::default(),
            poller: Default::default(),
            bindings: Default::default(),
            connections: Default::default(),
            ingress: Default::default(),
            egress: Default::default(),
        };

        network.poller.net.borrow_mut().replace(network.clone());
        network
    }

    /// Listen on a local endpoint
    pub fn bind(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Result<SocketHandle> {
        let handle = self.stack.bind(protocol, endpoint)?;
        self.bindings.borrow_mut().insert(handle);
        Ok(handle)
    }

    /// Stop listening on a local endpoint
    pub fn unbind(&self, protocol: Protocol, endpoint: impl Into<SocketEndpoint>) -> Result<()> {
        let handle = self.stack.unbind(protocol, endpoint)?;
        self.bindings.borrow_mut().remove(&handle);
        Ok(())
    }

    /// Returns whether a socket is bound on an endpoint
    pub fn get_bound(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Option<SocketHandle> {
        let endpoint = endpoint.into();
        let sockets = self.stack.sockets();
        let sockets_ref = sockets.borrow_mut();
        sockets_ref
            .iter()
            .find(|s| s.protocol() == protocol && s.local_endpoint() == endpoint)
            .map(|s| s.handle())
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
        async move {
            let connection = match tokio::time::timeout(timeout, connect).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(error)) => return Err(error),
                _ => return Err(Error::ConnectionTimeout),
            };
            connections
                .borrow_mut()
                .insert(connection.into(), connection);
            Ok(connection)
        }
        .boxed_local()
    }

    #[inline(always)]
    pub fn is_connected(&self, meta: &ConnectionMeta) -> bool {
        self.connections.borrow().contains_key(meta)
    }

    fn add_connection(&self, connection: Connection) {
        self.connections
            .borrow_mut()
            .insert(connection.into(), connection);
    }

    fn close_connection(&self, meta: &ConnectionMeta) -> Option<Connection> {
        self.connections.borrow_mut().remove(meta)
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

            async move {
                for chunk in vec.chunks(MAX_TCP_PAYLOAD_SIZE) {
                    let net = net.clone();
                    stack.send(chunk.to_vec(), conn, move || net.poll()).await?;
                }
                Ok(())
            }
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

        let sockets_rfc = self.stack.sockets();
        let mut sockets = sockets_rfc.borrow_mut();
        let mut events = Vec::new();
        let mut remove = Vec::new();

        for mut socket_ref in (*sockets).iter_mut() {
            let socket: &mut Socket = socket_ref.deref_mut();
            let handle = socket.handle();

            if socket.is_closed() {
                let protocol = socket.protocol();
                let desc = SocketDesc {
                    protocol,
                    local: socket.local_endpoint(),
                    remote: socket.remote_endpoint(),
                };

                if let Ok(meta) = desc.try_into() {
                    remove.push((meta, handle));
                    events.push(IngressEvent::Disconnected { desc });
                }
                continue;
            }

            while socket.can_recv() {
                let (remote, payload) = match socket.recv() {
                    Ok(Some(tuple)) => {
                        processed = true;
                        tuple
                    }
                    Ok(None) => break,
                    Err(err) => {
                        log::trace!("{}: ingress packet error: {}", *self.name, err);
                        processed = true;
                        continue;
                    }
                };

                let local = socket.local_endpoint();
                let protocol = socket.protocol();
                let desc = SocketDesc {
                    protocol,
                    local,
                    remote: remote.into(),
                };

                if self.bindings.borrow().contains(&handle) {
                    if let Ok(meta) = desc.try_into() {
                        if !self.is_connected(&meta) {
                            self.add_connection(Connection { handle, meta });
                            events.push(IngressEvent::InboundConnection { desc });
                        }
                    }
                }

                log::trace!("{}: ingress {} B IP packet", *self.name, payload.len());

                received += 1;
                let no = COUNTER.fetch_add(1, Ordering::Relaxed);
                events.push(IngressEvent::Packet { desc, payload, no });
            }
        }

        drop(sockets);
        remove.into_iter().for_each(|(meta, handle)| {
            self.close_connection(&meta);
            self.stack.close(meta.protocol, handle);

            let mut sockets = sockets_rfc.borrow_mut();
            sockets.remove(handle);
        });

        if !events.is_empty() {
            let ingress_tx = self.ingress.tx.clone();
            for event in events {
                if ingress_tx.send(event).is_err() {
                    log::trace!("{}: cannot receive packets: channel closed", *self.name);
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
        let mut events = Vec::new();

        while let Some(data) = device.next_phy_tx() {
            processed = true;

            match EtherFrame::try_from(data) {
                Ok(frame) => {
                    if let Some(event) = EgressEvent::from_frame(frame) {
                        sent += 1;
                        events.push(event);
                    }
                }
                Err(err) => {
                    log::trace!("{}: egress Ethernet frame error: {}", *self.name, err);
                    continue;
                }
            };
        }

        if !events.is_empty() {
            let egress_tx = self.egress.tx.clone();
            for event in events {
                if egress_tx.send(event).is_err() {
                    log::trace!("{}: cannot send packets: channel closed", *self.name);
                    break;
                }
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
}

impl EgressEvent {
    #[inline(always)]
    pub fn from_frame(frame: EtherFrame) -> Option<Self> {
        let remote = match &frame {
            EtherFrame::Ip(_) => {
                let packet = IpPacket::packet(frame.payload());
                packet.dst_address().into()
            }
            EtherFrame::Arp(_) => {
                let packet = ArpPacket::packet(frame.payload());
                packet.get_field(ArpField::TPA).into()
            }
        };

        Some(EgressEvent {
            remote,
            payload: frame.into(),
        })
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
    const MIN_INTERVAL: Duration = Duration::from_millis(10);
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
    use std::time::Duration;

    use futures::channel::{mpsc, oneshot};
    use futures::{Sink, SinkExt, Stream, StreamExt};
    use sha3::Digest;
    use smoltcp::iface::Route;
    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
    use tokio::task::spawn_local;

    use crate::{Connection, IngressEvent, Network, Protocol, Stack};

    const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

    fn new_network(ip: IpAddress) -> Network {
        let ipv4_cidr = IpCidr::new(ip.into(), 16);
        let route = match ip {
            IpAddress::Ipv4(ipv4) => Route::new_ipv4_gateway(ipv4),
            IpAddress::Ipv6(ipv6) => Route::new_ipv6_gateway(ipv6),
            _ => panic!("unspecified address"),
        };

        let stack = Stack::default();
        stack.add_address(ipv4_cidr);
        stack.add_route(ipv4_cidr, route);
        Network::new(ip, stack)
    }

    fn produce_data<S, E>(
        mut tx: S,
        total: usize,
        chunk_size: usize,
    ) -> oneshot::Receiver<anyhow::Result<Vec<u8>>>
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

            let _ = tx.close().await;

            println!("produced {} B", sent);
            match err {
                Some(e) => dtx.send(Err(e.into())),
                None => dtx.send(Ok(digest.result().to_vec())),
            }
        });

        drx
    }

    fn consume_data<S, A>(mut rx: S, total: usize) -> oneshot::Receiver<anyhow::Result<Vec<u8>>>
    where
        S: Stream<Item = A> + Unpin + 'static,
        A: AsRef<[u8]>,
    {
        let (dtx, drx) = oneshot::channel();

        spawn_local(async move {
            let mut digest = sha3::Sha3_224::new();
            let mut read: usize = 0;

            while let Some(vec) = rx.next().await {
                let len = vec.as_ref().len();

                read += len;
                digest.input(&vec);

                if read >= total {
                    break;
                }
            }

            println!("consumed {} B", read);
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
    async fn net_exchange(total: usize, chunk_size: usize) -> anyhow::Result<()> {
        println!(">> exchanging {} B in {} B chunks", total, chunk_size);

        let ip1 = Ipv4Address::new(10, 0, 0, 1);
        let ip2 = Ipv4Address::new(10, 0, 0, 2);

        let net1 = new_network(ip1.into());
        let net2 = new_network(ip2.into());

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

        let produced1 = produce1.await??;
        let produced2 = produce2.await??;
        let consumed1 = consume1.await??;
        let consumed2 = consume2.await??;

        assert_eq!(hex::encode(produced1), hex::encode(consumed2));
        assert_eq!(hex::encode(produced2), hex::encode(consumed1));

        Ok(())
    }

    async fn spawn_exchange(total: usize, chunk_size: usize) -> anyhow::Result<()> {
        tokio::task::LocalSet::new()
            .run_until(tokio::time::timeout(
                EXCHANGE_TIMEOUT,
                net_exchange(total, chunk_size),
            ))
            .await?
    }

    #[tokio::test]
    async fn exchange() -> anyhow::Result<()> {
        spawn_exchange(1024, 1).await?;
        spawn_exchange(1024, 4).await?;
        spawn_exchange(1024, 7).await?;
        spawn_exchange(10240, 16).await?;
        spawn_exchange(1024000, 383).await?;
        spawn_exchange(1024000, 384).await?;
        spawn_exchange(1024000, 4096).await?;
        spawn_exchange(1024000, 40960).await?;

        #[cfg(not(debug_assertions))]
        {
            spawn_exchange(10240000, 40960).await?;
            spawn_exchange(10240000, 131070).await?;
            spawn_exchange(10240000, 1024000).await?;
        }

        Ok(())
    }
}
