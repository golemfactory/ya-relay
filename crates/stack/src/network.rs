use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ops::DerefMut;
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use futures::FutureExt;
use smoltcp::socket::{Socket, SocketHandle};
use smoltcp::wire::IpEndpoint;
use tokio::time::Duration;

use crate::connection::{Connection, ConnectionMeta};
use crate::packet::{ArpField, ArpPacket, EtherFrame, IpPacket, PeekPacket};
use crate::protocol::Protocol;
use crate::socket::{SocketDesc, SocketExt};
use crate::stack::Stack;
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub enum IngressEvent {
    Packet { desc: SocketDesc, payload: Vec<u8> },
    InboundConnection { desc: SocketDesc },
    Disconnected { desc: SocketDesc },
}

#[derive(Clone, Debug)]
pub struct EgressEvent {
    pub remote: Box<[u8]>,
    pub payload: Box<[u8]>,
}

#[derive(Clone)]
pub struct Network {
    pub name: Rc<String>,
    stack: Stack<'static>,
    bindings: Rc<RefCell<HashSet<SocketHandle>>>,
    connections: Rc<RefCell<HashMap<ConnectionMeta, Connection>>>,
    ingress: Channel<IngressEvent>,
    egress: Channel<EgressEvent>,
}

impl Network {
    /// Creates a new Network instance
    pub fn new(name: String, stack: Stack<'static>) -> Self {
        Self {
            name: Rc::new(name),
            stack,
            bindings: Default::default(),
            connections: Default::default(),
            ingress: Default::default(),
            egress: Default::default(),
        }
    }

    /// Listen on a local endpoint
    pub fn bind(&self, protocol: Protocol, endpoint: impl Into<IpEndpoint>) -> Result<()> {
        let handle = self.stack.bind(protocol, endpoint.into())?;
        self.bindings.borrow_mut().insert(handle);
        Ok(())
    }

    #[inline(always)]
    fn is_bound(&self, handle: &SocketHandle) -> bool {
        self.bindings.borrow().contains(handle)
    }

    /// Initiate a TCP connection
    pub fn connect(
        &self,
        remote: IpEndpoint,
        timeout: Duration,
    ) -> LocalBoxFuture<Result<Connection>> {
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
    fn is_connected(&self, meta: &ConnectionMeta) -> bool {
        self.connections.borrow().contains_key(meta)
    }

    fn add_connection(&self, connection: Connection) {
        self.connections
            .borrow_mut()
            .insert(connection.into(), connection);
    }

    fn close_connection(&self, meta: &ConnectionMeta) -> Option<Connection> {
        { self.connections.borrow_mut().remove(meta) }.map(|connection| {
            self.stack
                .close(connection.meta.protocol, connection.handle);
            connection
        })
    }

    /// Inject send data into the stack
    #[inline(always)]
    pub fn send<B: Into<Vec<u8>>>(
        &self,
        data: B,
        connection: Connection,
    ) -> crate::connection::Send<'static> {
        self.stack.send(data, connection, || ())
    }

    /// Inject received data into the stack
    #[inline(always)]
    pub fn receive<B: Into<Vec<u8>>>(&self, data: B) {
        self.stack.receive(data)
    }

    /// Polls the inner network stack
    pub fn poll(&self) {
        loop {
            if let Err(err) = self.stack.poll() {
                log::warn!("{}: stack poll error: {}", *self.name, err);
            }

            let egress = self.process_egress();
            let ingress = self.process_ingress();

            if !egress && !ingress {
                break;
            }
        }
    }

    /// Take the ingress traffic receive channel
    #[inline(always)]
    pub fn ingress_receiver(&self) -> Option<unbounded_queue::Receiver<IngressEvent>> {
        self.ingress.receiver()
    }

    /// Take the egress traffic receive channel
    #[inline(always)]
    pub fn egress_receiver(&self) -> Option<unbounded_queue::Receiver<EgressEvent>> {
        self.egress.receiver()
    }

    fn process_ingress(&self) -> bool {
        let mut processed = false;

        let socket_rfc = self.stack.sockets();
        let mut sockets = socket_rfc.borrow_mut();
        let mut events = Vec::new();

        for mut socket_ref in (*sockets).iter_mut() {
            let socket: &mut Socket = socket_ref.deref_mut();
            let handle = socket.handle();

            if !socket.is_open() {
                let protocol = socket.protocol();
                let desc = SocketDesc {
                    protocol,
                    local: socket.local_endpoint(),
                    remote: socket.remote_endpoint(),
                };

                events.push(IngressEvent::Disconnected { desc });
                if let Ok(key) = desc.try_into() {
                    self.close_connection(&key);
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

                if self.is_bound(&handle) {
                    if let Ok(meta) = desc.try_into() {
                        if !self.is_connected(&meta) {
                            self.add_connection(Connection { handle, meta });
                            events.push(IngressEvent::InboundConnection { desc });
                        }
                    }
                }

                events.push(IngressEvent::Packet { desc, payload });
            }
        }

        if !(self.ingress.tx.is_closed() || events.is_empty()) {
            let mut ingress_tx = self.ingress.tx.clone();
            let _ = ingress_tx.send_all(events);
        }

        sockets.prune();
        processed
    }

    fn process_egress(&self) -> bool {
        let mut processed = false;

        let iface_rfc = self.stack.iface();
        let mut iface = iface_rfc.borrow_mut();
        let device = iface.device_mut();
        let mut payloads = Vec::new();

        while let Some(data) = device.next_phy_tx() {
            processed = true;

            let frame = match EtherFrame::try_from(data) {
                Ok(frame) => frame,
                Err(err) => {
                    log::trace!("{}: egress Ethernet frame error: {}", *self.name, err);
                    continue;
                }
            };

            let remote = match &frame {
                EtherFrame::Ip(_) => {
                    let packet = IpPacket::packet(frame.payload());
                    let dst_address = packet.dst_address();
                    log::trace!("{}: egress IP packet to {:02x?}", *self.name, dst_address);
                    dst_address.to_vec()
                }
                EtherFrame::Arp(_) => {
                    let packet = ArpPacket::packet(frame.payload());
                    let dst_address = packet.get_field(ArpField::TPA);
                    log::trace!("{}: egress ARP packet to {:02x?}", *self.name, dst_address);
                    dst_address.to_vec()
                }
            };

            payloads.push(EgressEvent {
                remote: remote.into(),
                payload: frame.into(),
            });
        }

        if !(self.egress.tx.is_closed() || payloads.is_empty()) {
            let mut egress_tx = self.egress.tx.clone();
            let _ = egress_tx.send_all(payloads);
        }

        processed
    }
}

#[derive(Clone)]
pub struct Channel<T> {
    pub tx: unbounded_queue::Sender<T>,
    rx: Rc<RefCell<Option<unbounded_queue::Receiver<T>>>>,
}

impl<T> Channel<T> {
    pub fn receiver(&self) -> Option<unbounded_queue::Receiver<T>> {
        self.rx.borrow_mut().take()
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        let (tx, rx) = unbounded_queue::queue_with_capacity(8);
        Self {
            tx,
            rx: Rc::new(RefCell::new(Some(rx))),
        }
    }
}
