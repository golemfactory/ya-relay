use anyhow::anyhow;
use chrono::Utc;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{NegativeMultiDecision, Quota, RateLimiter};
use std::collections::{BTreeSet, HashMap};
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{self};
use url::Url;

use crate::error::{BadRequest, Error, InternalError, NotFound, ServerResult, Unauthorized};
use crate::state::NodesState;

use ya_client_model::NodeId;

use crate::config::Config;
use crate::public_endpoints::EndpointsChecker;
use ya_relay_core::challenge::prepare_challenge_response;
use ya_relay_core::challenge::{self, CHALLENGE_DIFFICULTY};
use ya_relay_core::session::{NodeInfo, NodeSession, SessionId};
use ya_relay_core::udp_stream::{udp_bind, InStream, OutStream};
use ya_relay_core::{
    FORWARDER_RATE_LIMIT, FORWARDER_RESUME_INTERVAL, SESSION_CLEANER_INTERVAL, SESSION_TIMEOUT,
};
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::request::Kind;
use ya_relay_proto::proto::{RequestId, StatusCode};

#[derive(Clone)]
pub struct Server {
    pub state: Arc<RwLock<ServerState>>,
    pub inner: Arc<ServerImpl>,
}

pub struct ServerState {
    pub nodes: NodesState,
    pub starting_session: HashMap<SessionId, mpsc::Sender<proto::Request>>,
    resume_forwarding: BTreeSet<(QuantaInstant, SessionId, SocketAddr)>,

    recv_socket: Option<InStream>,
}

pub struct ServerImpl {
    pub socket: OutStream,
    pub url: Url,
    pub ip_checker: EndpointsChecker,
}

impl Server {
    pub async fn dispatch(&self, from: SocketAddr, packet: PacketKind) -> ServerResult<()> {
        let session_id = PacketKind::session_id(&packet);
        if !session_id.is_empty() {
            let id = SessionId::try_from(session_id.clone())
                .map_err(|_| Unauthorized::InvalidSessionId(session_id))?;
            let mut server = self.state.write().await;
            let _ = server.nodes.update_seen(id);
        }

        match packet {
            PacketKind::Packet(proto::Packet { kind, session_id }) => {
                // Empty `session_id` is sent to initialize Session, but only with Session request.
                if session_id.is_empty() {
                    match kind {
                        Some(proto::packet::Kind::Request(proto::Request {
                            request_id,
                            kind: Some(proto::request::Kind::Session(_)),
                        })) => {
                            return self.clone().new_session(request_id, from).await;
                        }
                        _ => return Err(BadRequest::NoSessionId.into()),
                    }
                }

                let id = SessionId::try_from(session_id.clone())
                    .map_err(|_| Unauthorized::InvalidSessionId(session_id))?;

                let node = match self.state.read().await.nodes.get_by_session(id) {
                    None => return self.clone().establish_session(id, from, kind).await,
                    Some(node) => node,
                };

                match kind {
                    Some(proto::packet::Kind::Request(proto::Request {
                        request_id,
                        kind: Some(kind),
                    })) => match kind {
                        Kind::Session(_) => {}
                        Kind::Register(_) => {}
                        Kind::Node(params) => {
                            self.node_request(request_id, id, from, params).await?
                        }
                        Kind::Slot(params) => {
                            self.slot_request(request_id, id, from, params).await?
                        }
                        Kind::Neighbours(params) => {
                            self.neighbours_request(request_id, id, from, params)
                                .await?
                        }
                        Kind::ReverseConnection(_) => {}
                        Kind::Ping(_) => self.ping_request(request_id, id, from).await?,
                    },
                    Some(proto::packet::Kind::Response(_)) => {
                        log::warn!(
                            "Server shouldn't get Response packet. Sender: {}, node {}",
                            from,
                            node.info.node_id,
                        );
                    }
                    Some(proto::packet::Kind::Control(_control)) => {
                        log::info!("Control packet from: {}", from);
                    }
                    _ => log::info!("Packet kind: None from: {}", from),
                }
            }

            PacketKind::Forward(forward) => self.forward(forward, from).await?,
            PacketKind::ForwardCtd(_) => {
                log::info!("ForwardCtd packet from: {}", from)
            }
        };

        Ok(())
    }

    async fn forward(&self, mut packet: proto::Forward, from: SocketAddr) -> ServerResult<()> {
        let session_id = SessionId::from(packet.session_id);
        let slot = packet.slot;

        let (src_node, dest_node) = {
            let server = self.state.read().await;

            // Authorization: Sending Node must have established session.
            // TODO: This operation has log(n) complexity. It wastes optimization in `get_by_slot`
            //       which has O(1) complexity.
            let src_node = server
                .nodes
                .get_by_session(session_id)
                .ok_or(Unauthorized::SessionNotFound(session_id))?;

            let dest_node = server
                .nodes
                .get_by_slot(slot)
                .ok_or(NotFound::NodeBySlot(slot))?;
            (src_node, dest_node)
        };
        if let Err(e) = src_node
            .forwarding_limiter
            .check_n(NonZeroU32::new(packet.payload.len().try_into().unwrap_or(u32::MAX)).unwrap())
        {
            log::trace!("Rate limiting: {:?}", e);
            match e {
                NegativeMultiDecision::InsufficientCapacity(cells) => {
                    // the query was invalid as the rate limite parameters can never accomodate the
                    // number of cells queried for.
                    log::warn!(
                        "Rate limited packet dropped. Exceeds limit. size: {}, from: {}",
                        cells,
                        &from
                    );
                }
                NegativeMultiDecision::BatchNonConforming(cells, retry_at) => {
                    log::debug!(
                        "Rate limited packet. size: {}, retry_at: {}",
                        cells,
                        retry_at
                    );
                    {
                        let mut server = self.state.write().await;
                        server.resume_forwarding.insert((
                            retry_at.earliest_possible(),
                            session_id,
                            from,
                        ));
                    }
                    let control_packet = proto::Packet::control(
                        session_id.to_vec(),
                        ya_relay_proto::proto::control::PauseForwarding { slot },
                    );
                    self.send_to(PacketKind::Packet(control_packet), &from)
                        .await
                        .map_err(|_| InternalError::Send)?;
                }
            }
            return Ok(());
        }

        // Replace destination slot and session id with sender slot and session_id.
        // Note: We can unwrap since SessionId type has the same array length.
        packet.slot = src_node.info.slot;
        packet.session_id = src_node.session.to_vec().as_slice().try_into().unwrap();

        log::debug!("Sending forward packet to {}", dest_node.address);

        self.send_to(PacketKind::Forward(packet), &dest_node.address)
            .await
            .map_err(|_| InternalError::Send)?;

        // TODO: We should update `last_seen` timestamp of `src_node`.
        Ok(())
    }

    async fn register_endpoints(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        _params: proto::request::Register,
        mut session: NodeSession,
    ) -> ServerResult<NodeSession> {
        // TODO: Note that we ignore endpoints sent by Node and only try
        //       to verify address, from which we received messages.

        let node_id = session.info.node_id;
        let endpoints = self
            .inner
            .ip_checker
            .public_endpoints(session_id, &from)
            .await?;

        for endpoint in endpoints.iter() {
            log::info!(
                "Discovered public endpoint {} for Node [{}]",
                endpoint.address,
                node_id
            )
        }

        session.info.endpoints.extend(endpoints.into_iter());

        let endpoints = session
            .info
            .endpoints
            .iter()
            .cloned()
            .map(proto::Endpoint::from)
            .collect();
        let response = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            proto::response::Register { endpoints },
        );

        self.send_to(response, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::debug!(
            "Responding to Register from Node: [{}], address: {}",
            node_id,
            from
        );
        Ok(session)
    }

    async fn ping_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
    ) -> ServerResult<()> {
        self.send_to(
            proto::Packet::response(
                request_id,
                session_id.to_vec(),
                proto::StatusCode::Ok,
                proto::response::Pong {},
            ),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::trace!("Responding to ping from: {}", from);

        Ok(())
    }

    async fn node_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        params: proto::request::Node,
    ) -> ServerResult<()> {
        if params.node_id.len() != 20 {
            return Err(BadRequest::InvalidNodeId.into());
        }
        let node_id = NodeId::from(&params.node_id[..]);

        log::debug!("{} requested Node [{}] info.", from, node_id);

        let node_info = {
            match self.state.read().await.nodes.get_by_node_id(node_id) {
                None => return Err(NotFound::Node(node_id).into()),
                Some(session) => session,
            }
        };

        self.node_response(request_id, session_id, from, node_info, params.public_key)
            .await
    }

    async fn neighbours_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        params: proto::request::Neighbours,
    ) -> ServerResult<()> {
        let nodes = {
            self.state
                .read()
                .await
                .nodes
                .neighbours(session_id, params.count)?
        };

        let nodes = nodes
            .into_iter()
            .map(|node_info| to_node_response(node_info, params.public_key))
            .collect();

        self.send_to(
            proto::Packet::response(
                request_id,
                session_id.to_vec(),
                proto::StatusCode::Ok,
                proto::response::Neighbours { nodes },
            ),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!(
            "Neighborhood sent to (request: {}): {}, session: {}",
            request_id,
            from,
            session_id
        );
        Ok(())
    }

    async fn slot_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        params: proto::request::Slot,
    ) -> ServerResult<()> {
        let node_info = {
            match self.state.read().await.nodes.get_by_slot(params.slot) {
                None => {
                    log::error!("Node by slot {} not found.", params.slot);
                    return Err(NotFound::NodeBySlot(params.slot).into());
                }
                Some(session) => session,
            }
        };

        self.node_response(request_id, session_id, from, node_info, params.public_key)
            .await
    }

    async fn node_response(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        node_info: NodeSession,
        public_key: bool,
    ) -> ServerResult<()> {
        let node_id = node_info.info.node_id;
        let node = to_node_response(node_info, public_key);

        self.send_to(
            proto::Packet::response(request_id, session_id.to_vec(), proto::StatusCode::Ok, node),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!(
            "Node [{}] info sent to (request: {}): {}",
            node_id,
            request_id,
            from
        );

        Ok(())
    }

    async fn new_session(self, request_id: RequestId, with: SocketAddr) -> ServerResult<()> {
        let (sender, receiver) = mpsc::channel(1);
        let session_id = SessionId::generate();

        log::info!("Initializing new session: {}", session_id);

        {
            self.state
                .write()
                .await
                .starting_session
                .entry(session_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        // TODO: We should spawn in different thread, but it's impossible since
        //       `init_session` starts actor and tokio panics.
        tokio::task::spawn_local(async move {
            if let Err(e) = self
                .clone()
                .init_session(with, request_id, session_id, receiver)
                .await
            {
                log::warn!("Error initializing session [{}], {}", session_id, e);

                // Establishing session failed.
                self.error_response(request_id, session_id.to_vec(), &with, e)
                    .await;
                self.cleanup_initialization(&session_id).await;
            }
        });
        Ok(())
    }

    async fn establish_session(
        self,
        id: SessionId,
        _from: SocketAddr,
        packet: Option<proto::packet::Kind>,
    ) -> ServerResult<()> {
        let mut sender = {
            match { self.state.read().await.starting_session.get(&id).cloned() } {
                Some(sender) => sender.clone(),
                None => return Err(Unauthorized::SessionNotFound(id).into()),
            }
        };

        let request = match packet {
            Some(proto::packet::Kind::Request(request)) => request,
            _ => return Err(BadRequest::InvalidPacket(id, "Request".to_string()).into()),
        };

        Ok(sender
            .send(request)
            .await
            .map_err(|_| InternalError::Send)?)
    }

    async fn init_session(
        self,
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        mut rc: mpsc::Receiver<proto::Request>,
    ) -> ServerResult<()> {
        let (packet, raw_challenge) = prepare_challenge_response();
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        self.send_to(challenge, &with)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Challenge sent to: {}", with);

        let node = match rc.next().await {
            Some(proto::Request {
                request_id,
                kind: Some(proto::request::Kind::Session(session)),
            }) => {
                log::info!("Got challenge from node: {}", with);

                // Validate the challenge
                if !challenge::verify(
                    &raw_challenge,
                    CHALLENGE_DIFFICULTY,
                    &session.challenge_resp,
                    session.public_key.as_slice(),
                )
                .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?
                {
                    return Err(Unauthorized::InvalidChallenge.into());
                }

                if session.node_id.len() != 20 {
                    return Err(BadRequest::InvalidNodeId.into());
                }

                let node_id = NodeId::from(&session.node_id[..]);
                let info = NodeInfo {
                    node_id,
                    public_key: session.public_key,
                    slot: u32::MAX,
                    endpoints: vec![],
                };

                let node = NodeSession {
                    info,
                    address: with,
                    session: session_id,
                    last_seen: Utc::now(),
                    forwarding_limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                        NonZeroU32::new(*FORWARDER_RATE_LIMIT).ok_or_else(|| {
                            InternalError::RateLimiterInit(format!(
                                "Invalid non zero value: {}",
                                *FORWARDER_RATE_LIMIT
                            ))
                        })?,
                    ))),
                };

                self.send_to(
                    proto::Packet::response(
                        request_id,
                        session_id.to_vec(),
                        proto::StatusCode::Ok,
                        proto::response::Session::default(),
                    ),
                    &with,
                )
                .await
                .map_err(|_| InternalError::Send)?;

                log::info!(
                    "Session: {}. Got valid challenge from node: {}",
                    session_id,
                    node_id
                );

                node
            }
            _ => return Err(BadRequest::InvalidPacket(session_id, "Session".to_string()).into()),
        };

        loop {
            match rc.next().await {
                Some(proto::Request {
                    request_id,
                    kind: Some(proto::request::Kind::Register(registration)),
                }) => {
                    log::trace!(
                        "Got register from Node: [{}] (request {})",
                        with,
                        request_id
                    );

                    let node_id = node.info.node_id;
                    let node = self
                        .register_endpoints(request_id, session_id, with, registration, node)
                        .await?;

                    self.cleanup_initialization(&session_id).await;

                    {
                        let mut server = self.state.write().await;
                        server.nodes.register(node);
                    }

                    log::info!(
                        "Session: {} established with Node: [{}]",
                        session_id,
                        node_id
                    );
                    break;
                }
                Some(proto::Request {
                    kind: Some(proto::request::Kind::Ping(_)),
                    ..
                }) => continue,
                _ => {
                    return Err(
                        BadRequest::InvalidPacket(session_id, "Register".to_string()).into(),
                    );
                }
            };
        }
        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        self.state.write().await.starting_session.remove(session_id);
    }

    async fn session_cleaner(&self) {
        log::debug!(
            "Starting session cleaner at interval {}s",
            (*SESSION_CLEANER_INTERVAL).as_secs()
        );
        let mut interval = time::interval(*SESSION_CLEANER_INTERVAL);
        loop {
            interval.tick().await;
            let s = self.clone();
            log::trace!("Cleaning up abandoned sessions");
            s.check_session_timeouts().await;
        }
    }

    async fn check_session_timeouts(&self) {
        let mut server = self.state.write().await;
        server.nodes.check_timeouts(*SESSION_TIMEOUT);
    }

    async fn forward_resumer(&self) {
        let mut interval = time::interval(*FORWARDER_RESUME_INTERVAL);
        loop {
            interval.tick().await;
            self.check_resume_forwarding().await;
        }
    }

    async fn check_resume_forwarding(&self) {
        let clock = DefaultClock::default();
        let mut to_resume = Vec::new();
        {
            let mut server = self.state.write().await;

            // First iteration to release write lock as soon as possible
            // FIXME: Use .drain_filter() on (resume_at <= now) when it becomes stable
            for elem in server.resume_forwarding.iter() {
                let (resume_at, session_id, socket_addr) = elem;
                let now = clock.now();
                if resume_at > &now {
                    let elem = *elem;
                    let mut split = server.resume_forwarding.split_off(&elem);
                    std::mem::swap(&mut split, &mut server.resume_forwarding);
                    break;
                }
                if let Some(node_session) = server.nodes.get_by_session(*session_id) {
                    to_resume.push((node_session, *session_id, *socket_addr));
                }
            }
        };

        // Second iteration without locks
        for (node_session, session_id, socket_addr) in to_resume {
            let control_packet = proto::Packet::control(
                session_id.to_vec(),
                ya_relay_proto::proto::control::ResumeForwarding {
                    slot: node_session.info.slot,
                },
            );
            if let Err(e) = self
                .send_to(PacketKind::Packet(control_packet), &socket_addr)
                .await
            {
                log::warn!("Can not send ResumeForwarding. {}", e);
            }
        }
    }

    async fn send_to(
        &self,
        packet: impl Into<PacketKind>,
        target: &SocketAddr,
    ) -> anyhow::Result<()> {
        Ok(self
            .inner
            .socket
            .clone()
            .send((packet.into(), *target))
            .await?)
    }

    pub async fn bind_udp(config: Config) -> anyhow::Result<Server> {
        let (input, output, addr) = udp_bind(&config.address).await?;
        let url = Url::parse(&format!("udp://{}:{}", addr.ip(), addr.port()))?;

        Server::bind(config, url, input, output).await
    }

    pub async fn bind(
        config: Config,
        addr: url::Url,
        input: InStream,
        output: OutStream,
    ) -> anyhow::Result<Server> {
        let inner = Arc::new(ServerImpl {
            socket: output,
            ip_checker: EndpointsChecker::spawn(config)
                .await
                .map_err(|e| anyhow!("Public endpoints checker initialization failed: {}", e))?,
            url: addr,
        });

        let state = Arc::new(RwLock::new(ServerState {
            nodes: NodesState::new(),
            starting_session: Default::default(),
            recv_socket: Some(input),
            resume_forwarding: BTreeSet::new(),
        }));

        Ok(Server { state, inner })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let server = self.clone();
        let server_session_cleaner = self.clone();
        let server_forward_resumer = self.clone();
        let mut input = {
            self.state
                .write()
                .await
                .recv_socket
                .take()
                .ok_or_else(|| anyhow::anyhow!("Server already running."))?
        };
        tokio::task::spawn_local(async move { server_session_cleaner.session_cleaner().await });
        tokio::task::spawn_local(async move { server_forward_resumer.forward_resumer().await });

        while let Some((packet, addr)) = input.next().await {
            let request_id = PacketKind::request_id(&packet);
            let session_id = PacketKind::session_id(&packet);
            let error = match server.dispatch(addr, packet).await {
                Ok(_) => continue,
                Err(e) => e,
            };

            log::error!("Packet dispatch failed. {}", error);
            if let Some(request_id) = request_id {
                server
                    .error_response(request_id, session_id, &addr, error)
                    .await;
            }
        }

        log::info!("Server stopped.");
        Ok(())
    }

    async fn error_response(&self, req_id: u64, id: Vec<u8>, addr: &SocketAddr, error: Error) {
        let status_code = match error {
            Error::Undefined(_) => StatusCode::Undefined,
            Error::BadRequest(_) => StatusCode::BadRequest,
            Error::Unauthorized(_) => StatusCode::Unauthorized,
            Error::NotFound(_) => StatusCode::NotFound,
            Error::Timeout(_) => StatusCode::Timeout,
            Error::Conflict(_) => StatusCode::Conflict,
            Error::PayloadTooLarge(_) => StatusCode::PayloadTooLarge,
            Error::TooManyRequests(_) => StatusCode::TooManyRequests,
            Error::Internal(_) => StatusCode::ServerError,
            Error::GatewayTimeout(_) => StatusCode::GatewayTimeout,
        };

        self.send_to(proto::Packet::error(req_id, id, status_code), addr)
            .await
            .map_err(|e| log::error!("Failed to send error response. {}.", e))
            .ok();
    }
}

pub fn dispatch_response(packet: PacketKind) -> Result<proto::response::Kind, proto::StatusCode> {
    match packet {
        PacketKind::Packet(proto::Packet {
            kind: Some(proto::packet::Kind::Response(proto::Response { kind, code, .. })),
            ..
        }) => match kind {
            None => {
                Err(proto::StatusCode::from_i32(code as i32).ok_or(proto::StatusCode::Undefined)?)
            }
            Some(response) => {
                match proto::StatusCode::from_i32(code as i32) {
                    Some(proto::StatusCode::Ok) => Ok(response),
                    _ => Err(proto::StatusCode::from_i32(code as i32)
                        .ok_or(proto::StatusCode::Undefined)?),
                }
            }
        },
        _ => Err(proto::StatusCode::Undefined),
    }
}

pub fn to_node_response(node_info: NodeSession, public_key: bool) -> proto::response::Node {
    let public_key = match public_key {
        true => node_info.info.public_key,
        false => vec![],
    };

    proto::response::Node {
        node_id: node_info.info.node_id.into_array().to_vec(),
        public_key,
        endpoints: node_info
            .info
            .endpoints
            .into_iter()
            .map(proto::Endpoint::from)
            .collect(),
        seen_ts: node_info.last_seen.timestamp() as u32,
        slot: node_info.info.slot,
    }
}
