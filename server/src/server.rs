use std::collections::{BTreeSet, HashMap};
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use chrono::Utc;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt, TryFutureExt};
use governor::clock::{Clock, DefaultClock, QuantaInstant};
use governor::{NegativeMultiDecision, Quota, RateLimiter};
use metrics::{counter, histogram};
use tokio::sync::RwLock;
use tokio::time;
use url::Url;

use crate::config::Config;
use crate::error::{BadRequest, Error, InternalError, NotFound, ServerResult, Unauthorized};
use crate::metrics::elapsed_metric;
use crate::public_endpoints::EndpointsChecker;
use crate::state::NodesState;

use ya_relay_core::challenge::{self, ChallengeDigest, CHALLENGE_DIFFICULTY};
use ya_relay_core::server_session::{LastSeen, NodeInfo, NodeSession, RequestHistory, SessionId};
use ya_relay_core::udp_stream::{udp_bind, InStream, OutStream};
use ya_relay_core::utils::{to_udp_url, ResultExt};
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::control::Disconnected;
use ya_relay_proto::proto::request::Kind;
use ya_relay_proto::proto::{RequestId, StatusCode};

#[derive(Clone)]
pub struct Server {
    pub state: Arc<RwLock<ServerState>>,
    pub inner: Arc<ServerImpl>,
    pub config: Arc<Config>,
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

        log::trace!(
            "[dispatch]: from: {} packet: {:?} session_id: [{}]",
            from,
            packet,
            hex::encode(&session_id)
        );

        if !session_id.is_empty() {
            let id = SessionId::try_from(session_id.clone())
                .map_err(|_| Unauthorized::InvalidSessionId(session_id.clone()))?;
            let server = self.state.read().await;
            let _ = server.nodes.update_seen(id);

            // Retries on the client-side might cause multiple packets with the same
            // request id to arrive, don't respond to any but the first one.
            //
            // This implementation may yield false negatives.
            if let Some(req_id) = packet.request_id() {
                if let Ok(true) = server.nodes.check_request_duplicate(id, req_id) {
                    return Ok(());
                }
            }
        }

        match packet {
            PacketKind::Packet(proto::Packet { kind, session_id }) => {
                log::debug!("[dispatch] PacketKind::Packet");

                // Empty `session_id` is sent to initialize Session, but only with Session request.
                if session_id.is_empty() {
                    return match kind {
                        Some(proto::packet::Kind::Request(proto::Request {
                            request_id,
                            kind: Some(proto::request::Kind::Session(_)),
                        })) => self.clone().new_session(request_id, from).await,
                        _ => Err(BadRequest::NoSessionId.into()),
                    };
                }

                let id = SessionId::try_from(session_id.clone())
                    .map_err(|_| Unauthorized::InvalidSessionId(session_id))?;

                log::debug!("[dispatch] session_id = {:?}", id);

                let node = match { self.state.read().await.nodes.get_by_session(id) } {
                    Some(node) => {
                        log::debug!("[dispatch] found node");
                        node
                    }
                    None => {
                        return match kind {
                            Some(proto::packet::Kind::Request(request)) => {
                                self.clone().establish_session(id, from, request).await
                            }
                            Some(proto::packet::Kind::Control(proto::Control { kind })) => {
                                match kind {
                                    Some(proto::control::Kind::Disconnected { .. }) => Ok(()),
                                    Some(proto::control::Kind::ResumeForwarding { .. }) => Ok(()),
                                    _ => unknown_session(id),
                                }
                            }
                            _ => unknown_session(id),
                        };
                    }
                };

                match kind {
                    Some(proto::packet::Kind::Request(proto::Request {
                        request_id,
                        kind: Some(kind),
                    })) => match kind {
                        Kind::Session(_) => {}
                        Kind::Register(_) => {}
                        Kind::Node(params) => {
                            counter!("ya-relay.packet.node-info", 1);
                            self.node_request(request_id, id, from, params)
                                .await
                                .on_error(|_| counter!("ya-relay.packet.node-info.error", 1))
                                .on_done(|_| counter!("ya-relay.packet.node-info.done", 1))?
                        }
                        Kind::Slot(params) => {
                            counter!("ya-relay.packet.slot-info", 1);
                            self.slot_request(request_id, id, from, params)
                                .await
                                .on_error(|_| counter!("ya-relay.packet.slot-info.error", 1))
                                .on_done(|_| counter!("ya-relay.packet.slot-info.done", 1))?
                        }
                        Kind::Neighbours(params) => {
                            counter!("ya-relay.packet.neighborhood", 1);
                            self.neighbours_request(request_id, id, from, params)
                                .await
                                .on_error(|_| counter!("ya-relay.packet.neighborhood.error", 1))
                                .on_done(|_| counter!("ya-relay.packet.neighborhood.done", 1))?
                        }
                        Kind::ReverseConnection(params) => {
                            counter!("ya-relay.packet.reverse-connection", 1);
                            self.reverse_request(request_id, id, from, params)
                                .await
                                .on_error(|_| {
                                    counter!("ya-relay.packet.reverse-connection.error", 1)
                                })
                                .on_done(|_| {
                                    counter!("ya-relay.packet.reverse-connection.done", 1)
                                })?
                        }
                        Kind::Ping(_) => {
                            counter!("ya-relay.packet.ping", 1);
                            self.ping_request(request_id, id, from)
                                .await
                                .on_error(|_| counter!("ya-relay.packet.ping.error", 1))
                                .on_done(|_| counter!("ya-relay.packet.ping.done", 1))?
                        }
                    },
                    Some(proto::packet::Kind::Response(_)) => {
                        log::warn!(
                            "Server shouldn't get Response packet. Sender: {from}, node {}",
                            node.info.default_node_id(),
                        );
                    }
                    Some(proto::packet::Kind::Control(packet)) => {
                        counter!("ya-relay.packet.disconnect", 1);
                        self.control(id, packet, from)
                            .await
                            .on_error(|_| counter!("ya-relay.packet.disconnect.error", 1))
                            .on_done(|_| counter!("ya-relay.packet.disconnect.done", 1))?
                    }
                    _ => log::info!("Packet kind: None from: {}", from),
                }
            }

            PacketKind::Forward(forward) => {
                log::debug!("[dispatch] PacketKind::Forward");

                counter!("ya-relay.packet.forward", 1);
                self.forward(forward, from)
                    .await
                    .on_error(|_| counter!("ya-relay.packet.forward.error", 1))
                    .on_done(|_| counter!("ya-relay.packet.forward.done", 1))?
            }
            PacketKind::ForwardCtd(_) => {
                log::debug!("[dispatch] PacketKind::ForwardCtd");

                log::info!("ForwardCtd packet from: {}", from)
            }
        };

        Ok(())
    }

    async fn forward(&self, mut packet: proto::Forward, from: SocketAddr) -> ServerResult<()> {
        let session_id = SessionId::from(packet.session_id);
        let slot = packet.slot;

        counter!(
            "ya-relay.packet.forward.incoming.size",
            packet.payload.len() as u64
        );

        let (src_node, dest_node) = {
            let server = self.state.read().await;

            // Authorization: Sending Node must have established session.
            let src_node = server
                .nodes
                .get_by_session(session_id)
                .ok_or(Unauthorized::SessionNotFound(session_id))
                .map_err(|e| {
                    log::trace!("SessionNotFound: {:?}", session_id);
                    e
                })?;

            let dest_node = match server
                .nodes
                .get_by_slot(slot)
                .ok_or(NotFound::NodeBySlot(slot))
            {
                Ok(dest_node) => dest_node,
                Err(e) => {
                    // Node probably won't notice, that his packets aren't reaching destination
                    // before TCP connection timeouts. It gets worse in case of unreliable forwarding.
                    // This is due to the fact, that client has no reason to get Node info again.
                    // That's why we must notify him.
                    //
                    // We could send Disconnected message, when we discover, that Node stopped responding
                    // to ping, but we would have to spam all Nodes in the network in this case. It is better
                    // to notify only interested Nodes, that means Nodes forwarding something.
                    let control_packet = proto::Packet::control(
                        session_id.to_vec(),
                        ya_relay_proto::proto::control::Disconnected {
                            by: Some(proto::control::disconnected::By::Slot(slot)),
                        },
                    );

                    drop(server);
                    self.send_to(PacketKind::Packet(control_packet), &from)
                        .await
                        .map_err(|_| InternalError::Send)?;

                    log::trace!(
                        "Sent Disconnected [slot={slot}] message to Node: {}.",
                        src_node.info.default_node_id()
                    );

                    return Err(e.into());
                }
            };
            (src_node, dest_node)
        };

        if let Err(e) = src_node
            .forwarding_limiter
            .check_n(NonZeroU32::new(packet.payload.len().try_into().unwrap_or(u32::MAX)).unwrap())
        {
            log::trace!("Rate limiting: {:?}", e);
            match e {
                NegativeMultiDecision::InsufficientCapacity(cells) => {
                    // the query was invalid as the rate limit parameters can never accommodate the
                    // number of cells queried for.
                    log::warn!(
                        "Rate limited packet dropped. Exceeds limit. size: {cells}, from: {from}"
                    );
                }
                NegativeMultiDecision::BatchNonConforming(cells, retry_at) => {
                    log::debug!("Rate limited packet. size: {cells}, retry_at: {retry_at}");
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

        log::trace!(
            "Sending forward packet from [{}] to [{}] ({} B)",
            src_node.info.default_node_id(),
            dest_node.info.default_node_id(),
            packet.payload.len(),
        );

        counter!(
            "ya-relay.packet.forward.outgoing.size",
            packet.payload.len() as u64
        );

        self.send_to(PacketKind::Forward(packet), &dest_node.address)
            .await
            .map_err(|_| InternalError::Send)?;
        Ok(())
    }

    async fn control(
        &self,
        session_id: SessionId,
        packet: proto::Control,
        from: SocketAddr,
    ) -> ServerResult<()> {
        if let proto::Control {
            kind: Some(proto::control::Kind::Disconnected(Disconnected { by: Some(by) })),
        } = packet
        {
            let mut server = self.state.write().await;
            match by {
                By::SessionId(_id) => match server.nodes.get_by_session(session_id) {
                    None => return Err(Unauthorized::SessionNotFound(session_id).into()),
                    Some(node) => {
                        log::info!(
                            "Received Disconnected message from Node [{}]({}).",
                            node.info.default_node_id(),
                            from
                        );
                        server.nodes.remove_session(node.info.slot)
                    }
                },
                // Allowing only closing sessions. Otherwise we could close someone's else session.
                By::Slot(_) => {
                    return Err(
                        BadRequest::InvalidParam("Slot. Only Session allowed".into()).into(),
                    )
                }
                By::NodeId(_) => {
                    return Err(
                        BadRequest::InvalidParam("NodeId. Only Session allowed".into()).into(),
                    )
                }
            };
        }
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

        let node_id = session.info.default_node_id();
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
            StatusCode::Ok,
            proto::response::Register { endpoints },
        );

        self.send_to(response, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::debug!("Responding to Register from Node: [{node_id}], address: {from}");
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
                StatusCode::Ok,
                proto::response::Pong {},
            ),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::trace!(
            "[ping_request]: Responding to ping from: {} session_id {}",
            from,
            session_id
        );
        Ok(())
    }

    async fn node_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        params: proto::request::Node,
    ) -> ServerResult<()> {
        let node_id = (&params.node_id)
            .try_into()
            .map_err(|_| BadRequest::InvalidNodeId)?;

        log::debug!(
            "[node_request]: {} requested Node [{}] info.",
            from,
            node_id
        );

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
        let start = Utc::now();

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
            .collect::<Vec<_>>();
        let return_count = nodes.len();

        self.send_to(
            proto::Packet::response(
                request_id,
                session_id.to_vec(),
                StatusCode::Ok,
                proto::response::Neighbours { nodes },
            ),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!(
            "Neighborhood ({} node(s)) sent to (request: {}, count: {}): {}, session: {}",
            return_count,
            request_id,
            params.count,
            from,
            session_id
        );
        histogram!(
            "ya-relay.packet.neighborhood.processing-time",
            elapsed_metric(start)
        );
        Ok(())
    }

    async fn reverse_request(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        from: SocketAddr,
        params: proto::request::ReverseConnection,
    ) -> ServerResult<()> {
        let source_node_info = match { self.state.read().await.nodes.get_by_session(session_id) } {
            None => return Err(Unauthorized::SessionNotFound(session_id).into()),
            Some(node_id) => node_id.info,
        };
        if source_node_info.endpoints.is_empty() {
            return Err(BadRequest::NoPublicEndpoints.into());
        }

        let target_node_id = (&params.node_id)
            .try_into()
            .map_err(|_| BadRequest::InvalidNodeId)?;
        let target_session = {
            match { self.state.read().await.nodes.get_by_node_id(target_node_id) } {
                None => {
                    return Err(InternalError::Generic(format!(
                        "There is no session with node_id {target_node_id}"
                    ))
                    .into())
                }
                Some(session) => session,
            }
        };

        let message_to_send = proto::Packet::control(
            target_session.session.to_vec(),
            proto::control::ReverseConnection {
                node_id: source_node_info.default_node_id().into_array().to_vec(),
                endpoints: source_node_info
                    .endpoints
                    .into_iter()
                    .map(proto::Endpoint::from)
                    .collect(),
            },
        );
        self.send_to(message_to_send, &target_session.address)
            .await
            .map_err(|_| InternalError::Send)?;

        let reponse = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            StatusCode::Ok,
            proto::response::ReverseConnection {},
        );

        self.send_to(reponse, &from)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!(
            "ReverseConnection sent. source: {}, target: {}, request: {}",
            &from,
            &target_session.address,
            &request_id,
        );
        log::debug!(
            "source_session: {}, target_session: {}",
            &session_id,
            &target_session.session
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
        let node_id = node_info.info.default_node_id();
        let node = to_node_response(node_info, public_key);

        self.send_to(
            proto::Packet::response(request_id, session_id.to_vec(), StatusCode::Ok, node),
            &from,
        )
        .await
        .map_err(|_| InternalError::Send)?;

        log::info!("Node [{node_id}] info sent to (request: {request_id}): {from}");
        Ok(())
    }

    async fn new_session(self, request_id: RequestId, with: SocketAddr) -> ServerResult<()> {
        let (sender, receiver) = mpsc::channel(1);
        let session_id = SessionId::generate();

        log::info!("Initializing new session: {session_id} with: {with}");
        counter!("ya-relay.session.establish.start", 1);

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
            let start_timestamp = Utc::now();

            if let Err(e) = self
                .clone()
                .init_session(with, request_id, session_id, receiver)
                .await
            {
                log::warn!("Error initializing session [{session_id}], {e}");
                counter!("ya-relay.session.establish.error", 1);

                // Establishing session failed.
                self.error_response(request_id, session_id.to_vec(), &with, e)
                    .await;
                self.cleanup_initialization(&session_id).await;
            }

            histogram!(
                "ya-relay.session.establish.time",
                elapsed_metric(start_timestamp)
            );
        });
        Ok(())
    }

    async fn establish_session(
        self,
        id: SessionId,
        _from: SocketAddr,
        request: proto::Request,
    ) -> ServerResult<()> {
        log::debug!("Server::establish_session");

        let mut sender = {
            match {
                log::debug!("[establish_session] get starting_session");
                self.state.read().await.starting_session.get(&id).cloned()
            } {
                Some(sender) => sender,
                None => return Err(Unauthorized::SessionNotFound(id).into()),
            }
        };

        log::debug!("[establish_session] spawn_local");
        tokio::task::spawn_local(async move {
            let _ = sender.send(request).await.map_err(|_| {
                log::warn!("Establish session channel closed. Dropping packet for session [{id}]")
            });
        });
        Ok(())
    }

    async fn init_session(
        self,
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        mut rc: mpsc::Receiver<proto::Request>,
    ) -> ServerResult<()> {
        const REQ_DEDUPLICATE_BUF_SIZE: usize = 16;

        let (packet, raw_challenge) = challenge::prepare_challenge_response(CHALLENGE_DIFFICULTY);
        let challenge =
            proto::Packet::response(request_id, session_id.to_vec(), StatusCode::Ok, packet);

        self.send_to(challenge, &with)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Challenge sent to: {with}, session: {session_id}");
        counter!("ya-relay.session.establish.challenge.sent", 1);

        let node = match rc.next().await {
            Some(proto::Request {
                request_id,
                kind: Some(proto::request::Kind::Session(session)),
            }) => {
                log::info!("Got challenge from node: {with}, session: {session_id}");

                // Validate the challenge
                let (node_id, identities) =
                    challenge::recover_identities_from_challenge::<ChallengeDigest>(
                        &raw_challenge,
                        CHALLENGE_DIFFICULTY,
                        session.challenge_resp,
                        None,
                    )
                    .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?;

                let info = NodeInfo {
                    identities,
                    slot: u32::MAX,
                    endpoints: vec![],
                    supported_encryption: vec![],
                };

                let node = NodeSession {
                    info,
                    address: with,
                    session: session_id,
                    last_seen: LastSeen::now(),
                    forwarding_limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                        NonZeroU32::new(self.config.forwarder_rate_limit).ok_or_else(|| {
                            InternalError::RateLimiterInit(format!(
                                "Invalid non zero value: {}",
                                self.config.forwarder_rate_limit
                            ))
                        })?,
                    ))),
                    request_history: RequestHistory::new(REQ_DEDUPLICATE_BUF_SIZE),
                };

                self.send_to(
                    proto::Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Ok,
                        proto::response::Session::default(),
                    ),
                    &with,
                )
                .await
                .map_err(|_| InternalError::Send)?;

                log::info!(
                    "Session: {session_id} ({with}). Got valid challenge from node: {node_id}"
                );
                counter!("ya-relay.session.establish.challenge.valid", 1);

                node
            }
            _ => return invalid_packet(session_id, "Session"),
        };

        loop {
            match rc.next().await {
                Some(proto::Request {
                    request_id,
                    kind: Some(proto::request::Kind::Register(registration)),
                }) => {
                    log::trace!("Got register from Node: [{with}] (request {request_id})");
                    counter!("ya-relay.session.establish.register", 1);

                    let node_id = node.info.default_node_id();
                    let node = self
                        .register_endpoints(request_id, session_id, with, registration, node)
                        .await?;

                    self.cleanup_initialization(&session_id).await;

                    {
                        let mut server = self.state.write().await;
                        server.nodes.register(node);
                    }

                    log::info!("Session: {session_id} established with Node: [{node_id}] ({with})");
                    counter!("ya-relay.session.establish.finished", 1);
                    break;
                }
                Some(proto::Request {
                    kind: Some(proto::request::Kind::Ping(_)),
                    ..
                }) => continue,
                _ => return invalid_packet(session_id, "Register"),
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
            (self.config.session_cleaner_interval).as_secs()
        );
        let mut interval = time::interval(self.config.session_cleaner_interval);
        loop {
            interval.tick().await;
            let s = self.clone();
            let start = Utc::now();

            log::trace!("[session_cleaner]: Cleaning up abandoned sessions");
            s.check_session_timeouts().await;
            log::trace!("[session_cleaner]: Session cleanup complete");

            histogram!(
                "ya-relay.session.cleaner.processing-time",
                elapsed_metric(start)
            );
        }
    }

    async fn check_session_timeouts(&self) {
        let mut server = self.state.write().await;
        server.nodes.check_timeouts(
            self.config.session_timeout,
            self.config.session_purge_timeout,
        );
    }

    async fn forward_resumer(&self) {
        let mut interval = time::interval(self.config.forwarder_resume_interval);
        loop {
            interval.tick().await;
            let start = Utc::now();

            self.check_resume_forwarding().await;

            histogram!(
                "ya-relay.forwarding-limiter.processing-time",
                elapsed_metric(start)
            );
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
        let url = to_udp_url(addr)?;

        Server::bind(config, url, input, output).await
    }

    pub async fn bind(
        config: Config,
        addr: Url,
        input: InStream,
        output: OutStream,
    ) -> anyhow::Result<Server> {
        let config = Arc::new(config);
        let inner = Arc::new(ServerImpl {
            socket: output,
            ip_checker: EndpointsChecker::spawn(config.clone())
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

        Ok(Server {
            state,
            inner,
            config,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        const DISPATCH_TIMEOUT: Duration = Duration::from_millis(3500);
        const DISPATCH_TASK_COUNT: usize = 32;

        let server = self.clone();
        let server_session_cleaner = self.clone();
        let server_forward_resumer = self.clone();
        let input = {
            self.state
                .write()
                .await
                .recv_socket
                .take()
                .ok_or_else(|| anyhow::anyhow!("Server already running."))?
        };
        tokio::task::spawn_local(async move { server_session_cleaner.session_cleaner().await });
        tokio::task::spawn_local(async move { server_forward_resumer.forward_resumer().await });

        input
            .map(|(packet, addr, timestamp)| {
                let server = server.clone();
                let request_id = PacketKind::request_id(&packet);
                let session_id = PacketKind::session_id(&packet);

                let fut = async move {
                    counter!("ya-relay.packet.incoming", 1);

                    log::debug!("[run] start processing");
                    if server.drop_policy(&packet, timestamp) {
                        counter!("ya-relay.packet.dropped", 1);
                        log::debug!(
                            "Packet from {addr} waited too long in queue ({timestamp}), dropping"
                        );
                        log::debug!("[run] dropped");
                        return Ok::<_, ()>(());
                    }

                    let start = Utc::now();

                    log::debug!("[run] run inner dispatch");
                    if let Err(error) = server.dispatch(addr, packet).await {
                        log::error!(
                            "Packet dispatch failed (request={request_id:?}, from={addr}). {error}"
                        );

                        log::debug!("[run] dispatch failed");

                        if let Some(request_id) = request_id {
                            server
                                .error_response(request_id, session_id, &addr, error)
                                .await;
                        }
                        counter!("ya-relay.packet.incoming.error", 1);
                    };

                    histogram!("ya-relay.packet.response-time", elapsed_metric(timestamp));
                    histogram!("ya-relay.packet.processing-time", elapsed_metric(start));

                    Ok(())
                };

                tokio::time::timeout(DISPATCH_TIMEOUT, fut).inspect_err(move |_| {
                    counter!("ya-relay.packet.timeout", 1);
                    log::error!("Packet dispatch timed out (request={request_id:?}, from={addr})");
                })
            })
            .buffer_unordered(DISPATCH_TASK_COUNT)
            .for_each(|_| futures::future::ready(()))
            .await;

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

pub fn dispatch_response(packet: PacketKind) -> Result<proto::response::Kind, StatusCode> {
    match packet {
        PacketKind::Packet(proto::Packet {
            kind: Some(proto::packet::Kind::Response(proto::Response { kind, code, .. })),
            ..
        }) => match kind {
            None => Err(StatusCode::from_i32(code).ok_or(StatusCode::Undefined)?),
            Some(response) => match StatusCode::from_i32(code) {
                Some(StatusCode::Ok) => Ok(response),
                _ => Err(StatusCode::from_i32(code).ok_or(StatusCode::Undefined)?),
            },
        },
        _ => Err(StatusCode::Undefined),
    }
}

pub fn to_node_response(node_info: NodeSession, public_key: bool) -> proto::response::Node {
    let identities = match public_key {
        true => node_info.info.identities.iter().map(Into::into).collect(),
        false => node_info
            .info
            .identities
            .iter()
            .map(|ident| proto::Identity {
                public_key: vec![],
                node_id: ident.node_id.into_array().to_vec(),
            })
            .collect(),
    };

    proto::response::Node {
        identities,
        endpoints: node_info
            .info
            .endpoints
            .into_iter()
            .map(proto::Endpoint::from)
            .collect(),
        seen_ts: node_info.last_seen.time().timestamp() as u32,
        slot: node_info.info.slot,
        supported_encryptions: node_info.info.supported_encryption,
    }
}

#[inline]
fn unknown_session<T>(session_id: SessionId) -> ServerResult<T> {
    Err(Unauthorized::SessionNotFound(session_id).into())
}

#[inline]
fn invalid_packet<T>(session_id: SessionId, expected: impl ToString) -> ServerResult<T> {
    Err(BadRequest::InvalidPacket(session_id, expected.to_string()).into())
}
