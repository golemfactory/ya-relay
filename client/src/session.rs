use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dispatch::{Dispatched, Dispatcher};
use crate::session_layer::SessionsLayer;

use ya_client_model::NodeId;
use ya_relay_core::challenge::{self, prepare_challenge_response, CHALLENGE_DIFFICULTY};
use ya_relay_core::error::{BadRequest, InternalError, ServerResult, Unauthorized};
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::utils::parse_node_id;
use ya_relay_proto::proto::{RequestId, SlotId};
use ya_relay_proto::{codec, proto};

const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);

/// Udp connection to other Node. It is either peer-to-peer connection
/// or central server session. Implements functions for sending
/// protocol messages or forwarding generic messages.
#[derive(Clone)]
pub struct Session {
    pub remote: SocketAddr,
    pub id: SessionId,

    sink: OutStream,
    dispatcher: Dispatcher,
}

impl Session {
    pub fn new(remote_addr: SocketAddr, id: SessionId, sink: OutStream) -> Arc<Self> {
        Arc::new(Self {
            remote: remote_addr,
            id,
            sink,
            dispatcher: Dispatcher::default(),
        })
    }

    pub fn dispatcher(&self) -> Dispatcher {
        self.dispatcher.clone()
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        log::info!("[{}] registering endpoints.", self.id);

        let response = self
            .request::<proto::response::Register>(
                proto::request::Register { endpoints }.into(),
                self.id.to_vec(),
                REGISTER_REQUEST_TIMEOUT,
            )
            .await?
            .packet;

        log::info!("[{}] endpoints registration finished.", self.id);

        Ok(response.endpoints)
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        log::debug!(
            "Finding Node info [{}], using session {}.",
            node_id,
            self.id
        );

        let packet = proto::request::Node {
            node_id: node_id.into_array().to_vec(),
            public_key: true,
        };
        self.find_node_by(packet).await
    }

    pub async fn find_slot(&self, slot: SlotId) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Slot {
            slot,
            public_key: true,
        };
        self.find_node_by(packet).await
    }

    async fn find_node_by(
        &self,
        packet: impl Into<proto::Request>,
    ) -> anyhow::Result<proto::response::Node> {
        let response = self
            .request::<proto::response::Node>(
                packet.into(),
                self.id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?
            .packet;
        Ok(response)
    }

    pub async fn neighbours(&self, count: u32) -> anyhow::Result<proto::response::Neighbours> {
        let packet = proto::request::Neighbours {
            count,
            public_key: true,
        };
        let neighbours = self
            .request::<proto::response::Neighbours>(
                packet.into(),
                self.id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?
            .packet;
        Ok(neighbours)
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let packet = proto::request::Ping {};
        self.request::<proto::response::Pong>(
            packet.into(),
            self.id.to_vec(),
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await?;

        Ok(())
    }
}

impl Session {
    pub(crate) async fn request<T>(
        &self,
        request: proto::Request,
        session_id: Vec<u8>,
        timeout: Duration,
    ) -> anyhow::Result<Dispatched<T>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let response = self.response::<T>(request.request_id, timeout);
        let packet = proto::Packet {
            session_id,
            kind: Some(proto::packet::Kind::Request(request)),
        };
        self.send(packet).await?;

        Ok(response.await?)
    }

    #[inline(always)]
    pub(crate) fn response<'a, T>(
        &self,
        request_id: RequestId,
        timeout: Duration,
    ) -> LocalBoxFuture<'a, anyhow::Result<Dispatched<T>>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        self.dispatcher.clone().response::<T>(request_id, timeout)
    }

    pub async fn send(&self, packet: impl Into<codec::PacketKind>) -> anyhow::Result<()> {
        let mut sink = self.sink.clone();
        Ok(sink.send((packet.into(), self.remote)).await?)
    }
}

#[derive(Clone)]
pub(crate) struct StartingSessions {
    state: Arc<Mutex<StartingSessionsState>>,

    layer: SessionsLayer,
    sink: OutStream,
}

#[derive(Default)]
struct StartingSessionsState {
    /// Initialization of session started by other peers.
    incoming_sessions: HashMap<SessionId, mpsc::Sender<(RequestId, proto::request::Session)>>,
    /// Temporary sessions stored during initialization period.
    /// After session is established, new struct in SessionsLayer is created
    /// and this one is removed.
    tmp_sessions: HashMap<SocketAddr, Arc<Session>>,
}

impl StartingSessions {
    pub fn new(layer: SessionsLayer, sink: OutStream) -> StartingSessions {
        StartingSessions {
            state: Arc::new(Mutex::new(StartingSessionsState::default())),
            sink,
            layer,
        }
    }

    pub fn temporary_session(&mut self, addr: SocketAddr) -> Arc<Session> {
        let session = Session::new(addr, SessionId::generate(), self.sink.clone());
        self.state
            .lock()
            .unwrap()
            .tmp_sessions
            .insert(addr, session.clone());
        session
    }

    pub fn remove_temporary_session(&mut self, addr: &SocketAddr) {
        self.state.lock().unwrap().tmp_sessions.remove(addr);
    }

    pub fn dispatcher(&self, addr: SocketAddr) -> Option<Dispatcher> {
        self.state
            .lock()
            .unwrap()
            .tmp_sessions
            .get(&addr)
            .map(|session| session.dispatcher.clone())
    }

    pub async fn dispatch_session(
        self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) {
        // This will be new session.
        match session_id.is_empty() {
            true => self.new_session(request_id, from, request).await,
            false => self
                .existing_session(session_id, request_id, from, request)
                .await
                .map_err(|e| e.into()),
        }
        .map_err(|e| log::warn!("{}", e))
        .ok();
    }

    pub async fn new_session(
        self,
        request_id: RequestId,
        with: SocketAddr,
        request: proto::request::Session,
    ) -> anyhow::Result<()> {
        let node_id = parse_node_id(&request.node_id)?;
        let session_id = SessionId::generate();
        let (sender, receiver) = mpsc::channel(1);

        log::info!(
            "Node {} ({}) tries to establish p2p session.",
            node_id,
            with
        );

        {
            self.state
                .lock()
                .unwrap()
                .incoming_sessions
                .entry(session_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        tokio::task::spawn_local(async move {
            if let Err(e) = self
                .clone()
                .init_session_handler(with, request_id, session_id, request, receiver)
                .await
            {
                log::warn!(
                    "Error initializing session {} with node {}. Error: {}",
                    session_id,
                    node_id,
                    e
                );

                // Establishing session failed.
                self.layer
                    .error_response(request_id, session_id.to_vec(), &with, e)
                    .await;
                self.cleanup_initialization(&session_id).await;
            }
        });
        Ok(())
    }

    async fn existing_session(
        &self,
        raw_id: Vec<u8>,
        request_id: RequestId,
        _from: SocketAddr,
        request: proto::request::Session,
    ) -> ServerResult<()> {
        let id = SessionId::try_from(raw_id.clone())
            .map_err(|_| Unauthorized::InvalidSessionId(raw_id))?;

        let mut sender = {
            match {
                self.state
                    .lock()
                    .unwrap()
                    .incoming_sessions
                    .get(&id)
                    .cloned()
            } {
                Some(sender) => sender.clone(),
                None => return Err(Unauthorized::SessionNotFound(id).into()),
            }
        };

        Ok(sender
            .send((request_id, request))
            .await
            .map_err(|_| InternalError::Send)?)
    }

    async fn init_session_handler(
        mut self,
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        request: proto::request::Session,
        mut rc: mpsc::Receiver<(RequestId, proto::request::Session)>,
    ) -> ServerResult<()> {
        let node_id = parse_node_id(&request.node_id)?;

        let (packet, raw_challenge) = prepare_challenge_response();
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        let tmp_session = self.temporary_session(with);

        tmp_session
            .send(challenge)
            .await
            .map_err(|_| InternalError::Send)?;

        log::debug!("Challenge sent to Node: [{}], address: {}", node_id, with);

        // Compute challenge in different thread to avoid blocking runtime.
        let challenge_handle = self.layer.solve_challenge(request).await;

        if let Some((request_id, session)) = rc.next().await {
            log::debug!(
                "Got challenge response from node: [{}], address: {}",
                node_id,
                with
            );

            // Validate the challenge
            let verification = challenge::verify(
                &raw_challenge,
                CHALLENGE_DIFFICULTY,
                &session.challenge_resp,
                session.public_key.as_slice(),
            )
            .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?;

            if !verification {
                return Err(Unauthorized::InvalidChallenge.into());
            }

            log::debug!(
                "Challenge from Node: [{}], address: {} verified.",
                node_id,
                with
            );

            let packet = proto::response::Session {
                challenge_resp: challenge_handle
                    .await
                    .map_err(|e| InternalError::Generic(e.to_string()))?,
                challenge_req: None,
                public_key: self.layer.config.public_key().await?.bytes().to_vec(),
            };

            tmp_session
                .send(proto::Packet::response(
                    request_id,
                    session_id.to_vec(),
                    proto::StatusCode::Ok,
                    packet,
                ))
                .await
                .map_err(|_| InternalError::Send)?;

            self.layer
                .add_incoming_session(with, session_id, node_id)
                .await
                .map_err(|e| InternalError::Generic(e.to_string()))?;

            log::info!(
                "Incoming P2P session {} with Node: [{}], address: {} established.",
                session_id,
                node_id,
                with
            );
        }

        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        let mut state = self.state.lock().unwrap();

        state.incoming_sessions.remove(session_id);
        let addr =
            state
                .tmp_sessions
                .iter()
                .find_map(|(_, session)| match session.id == *session_id {
                    true => Some(session.remote),
                    false => None,
                });

        if let Some(addr) = addr {
            state.tmp_sessions.remove(&addr);
        }
    }
}
