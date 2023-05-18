use anyhow::{anyhow, bail};
use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable, LocalBoxFuture};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use ya_relay_core::challenge::{self, ChallengeDigest, RawChallenge};
use ya_relay_core::crypto::Crypto;
use ya_relay_core::error::{InternalError, ServerResult, Unauthorized};
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::RequestId;
use ya_relay_stack::Protocol;

use crate::_error::{ProtocolError, RequestError, SessionError, SessionInitError, SessionResult};
use crate::_routing_session::DirectSession;
use crate::_session::RawSession;
use crate::_session_guard::{GuardedSessions, InitState, SessionState};
use crate::_session_layer::SessionLayer;
use crate::client::ClientConfig;

#[derive(Clone)]
pub(crate) struct SessionProtocol {
    state: Arc<Mutex<StartingSessionsState>>,

    guarded: GuardedSessions,
    layer: SessionLayer,

    simultaneous_challenges: Arc<Semaphore>,
    config: Arc<ClientConfig>,
    sink: OutStream,
}

#[derive(Default)]
struct StartingSessionsState {
    /// Initialization of session started by other peers.
    incoming_sessions: HashMap<SessionId, mpsc::Sender<(RequestId, proto::request::Session)>>,

    /// Collection of background tasks that must be stopped on shutdown.
    handles: Vec<AbortHandle>,
}

impl SessionProtocol {
    pub fn new(layer: SessionLayer, guarded: GuardedSessions, sink: OutStream) -> SessionProtocol {
        // We don't want to overwhelm CPU with challenge solving operations.
        // Leave at least 2 threads for yagna to work.
        let max_heavy_threads = std::cmp::max(num_cpus::get() - 2, 1);

        SessionProtocol {
            state: Arc::new(Mutex::new(StartingSessionsState::default())),
            sink,
            layer: layer.clone(),
            guarded,
            config: layer.config.clone(),
            simultaneous_challenges: Arc::new(Semaphore::new(max_heavy_threads)),
        }
    }

    /// Creates temporary session used only during initialization.
    /// Only one session will be created for one target Node address.
    /// If temporary session was already created, this function will wait for initialization finish.
    pub async fn temporary_session(&self, addr: SocketAddr) -> Arc<RawSession> {
        self.guarded
            .temporary_session(addr, self.sink.clone())
            .await
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
        .map_err(|e| log::warn!("{e}"))
        .ok();
    }

    async fn init_session(
        &self,
        addr: SocketAddr,
        node_id: NodeId,
        challenge: bool,
    ) -> SessionResult<Arc<DirectSession>> {
        let config = self.config.clone();
        let this_id = config.node_id;

        self.guarded
            .transition_outgoing(node_id, InitState::Initializing)
            .await?;

        let tmp_session = self.temporary_session(addr).await;

        log::debug!("[{this_id}] initializing session with {addr}");

        let (request, raw_challenge) = self.prepare_challenge_request(challenge).await?;
        let response = tmp_session
            .request::<proto::response::Session>(
                request.into(),
                vec![],
                self.config.session_request_timeout,
            )
            .await
            .map_err(ProtocolError::from)?;

        self.guarded
            .transition_outgoing(node_id, InitState::ChallengeReceived)
            .await?;

        let session_id = SessionId::try_from(response.session_id.clone())
            .map_err(|e| ProtocolError::InvalidResponse(e.to_string()))?;
        let challenge_req = response.packet.challenge_req.ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Expected ChallengeRequest while initializing session with {addr}".to_string(),
            )
        })?;
        let challenge_handle = self.solve_challenge(challenge_req).await;

        log::trace!("Solving challenge while establishing session with: {addr}");

        // with the current ECDSA scheme the public key
        // can be recovered from challenge signature
        let packet = proto::request::Session {
            challenge_resp: Some(
                challenge_handle
                    .await
                    .map_err(|e| SessionError::Internal(e.to_string()))?,
            ),
            ..Default::default()
        };

        let response = tmp_session
            .request::<proto::response::Session>(
                packet.into(),
                session_id.to_vec(),
                config.challenge_request_timeout,
            )
            .await
            .map_err(ProtocolError::from)?;

        self.guarded
            .transition_outgoing(node_id, InitState::ChallengeResponseSent)
            .await?;

        log::trace!("Challenge sent to: {addr}");

        if session_id != &response.session_id[..] {
            let _ = tmp_session.disconnect().await;
            return Err(ProtocolError::InvalidResponse(format!(
                "Session id mismatch: {} (expected) vs {:?} (response)",
                session_id, response.session_id,
            ))
            .into());
        }

        log::trace!("Validating challenge from: {addr}");

        let (remote_id, identities) = match {
            if challenge {
                // Validate the challenge
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    config.challenge_difficulty,
                    response.packet.challenge_resp,
                    Some(node_id),
                )
            } else {
                Ok(Default::default())
            }
        } {
            Ok(tuple) => tuple,
            Err(e) => {
                let _ = tmp_session.disconnect().await;
                return Err(ProtocolError::InvalidChallenge(format!("{e}")).into());
            }
        };

        self.guarded
            .transition_outgoing(node_id, InitState::ChallengeVerified)
            .await?;

        if remote_id != node_id && !identities.iter().any(|i| i.node_id == node_id) {
            let _ = tmp_session.disconnect().await;
            return Err(ProtocolError::InvalidResponse(format!(
                "Remote node id mismatch: {node_id} (expected) vs {remote_id} (response)",
            ))
            .into());
        }

        // We should be ready to receive messages from other party immediately
        // after we send ResumeForwarding. That's why we register session before.
        let session = self
            .layer
            .register_session(addr, session_id, remote_id, identities)
            .await
            .map_err(|e| {
                SessionError::Internal(format!("Failed to register session. Error: {e}"))
            })?;

        self.guarded
            .transition_outgoing(node_id, InitState::SessionRegistered)
            .await?;

        session
            .raw
            .send(proto::Packet::control(
                session.raw.id.to_vec(),
                ya_relay_proto::proto::control::ResumeForwarding::default(),
            ))
            .await
            .map_err(RequestError::from)
            .map_err(ProtocolError::from)?;

        self.guarded
            .transition(node_id, SessionState::Established)
            .await?;

        log::trace!("[{this_id}] session {session_id} established with address: {addr}");

        // Send ping to measure response time.
        // Otherwise we will have very big response time, when querying this information
        // directly after establishing session.
        let session_ = session.clone();
        tokio::task::spawn_local(async move {
            session_.raw.ping().await.ok();
        });

        Ok(session)
    }

    async fn init_p2p_session(
        &self,
        addr: SocketAddr,
        node_id: NodeId,
    ) -> Result<Arc<DirectSession>, SessionInitError> {
        log::info!("Initializing p2p session with Node: [{node_id}], address: {addr}");

        let session = self
            .init_session(addr, node_id, true)
            .await
            .map_err(|e| SessionInitError::P2P(node_id, e))?;

        log::info!(
            "Established P2P session {} with node [{}] ({})",
            session.raw.id,
            node_id,
            addr
        );

        Ok(session)
    }

    async fn init_server_session(
        &self,
        addr: SocketAddr,
    ) -> Result<Arc<DirectSession>, SessionInitError> {
        log::info!("Initializing session with NET relay server at: {addr}");

        // TODO: In the future relays should have own NodeId.
        let session = self
            .init_session(addr, NodeId::default(), false)
            .await
            .map_err(|e| SessionInitError::Relay(addr.clone(), e))?;

        log::info!(
            "Established session {} with NET relay server ({addr})",
            session.raw.id,
        );

        Ok(session)
    }

    /// TODO: Session guards should be handled on external layer.
    pub async fn new_session(
        self,
        request_id: RequestId,
        with: SocketAddr,
        request: proto::request::Session,
    ) -> anyhow::Result<()> {
        let session_id = SessionId::generate();
        let remote_id = challenge::recover_default_node_id(&request)?;
        let (sender, receiver) = mpsc::channel(1);

        let (abort_handle, abort_registration) = AbortHandle::new_pair();

        let this = self.clone();
        let this1 = this.clone();
        let this2 = this.clone();
        let init_future = Abortable::new(
            timeout(self.config.incoming_session_timeout, async move {
                // In case of ReverseConnection, we are awaiting incoming connection from
                // other party
                if this.guarded.try_access_unguarded(remote_id).await {
                    log::debug!("Node [{remote_id}] enters unguarded session initialization.");

                    this.init_session_handler(
                        with, request_id, session_id, remote_id, request, receiver,
                    )
                    .await
                } else {
                    let lock = this.guarded.guard_initialization(remote_id, &[with]).await;
                    let _guard = lock.write().await;

                    this.init_session_handler(
                        with, request_id, session_id, remote_id, request, receiver,
                    )
                    .await
                }
            }),
            abort_registration,
        )
        .map(move |result| match result {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_timeout)) => Err(SessionError::Timeout("".to_string())),
            Err(_aborted) => Err(SessionError::Internal("Aborted".to_string())),
        })
        .or_else(move |result| async move {
            if let Some(session) = this1.guarded.get_temporary_session(&with).await {
                session.disconnect().await.ok();
            }

            log::warn!(
                "Error initializing session {session_id} with node [{remote_id}] ({with}). Error: {result}"
            );

            // Establishing session failed.
            this1.cleanup_initialization(&session_id).await;
            Err(result)
        })
        .then(move |result| async move {
            this2.guarded.stop_guarding(remote_id, result).await;
        });

        {
            let mut state = self.state.lock().unwrap();
            state.incoming_sessions.entry(session_id).or_insert(sender);
            state.handles.push(abort_handle);
        }

        tokio::task::spawn_local(init_future);
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
        remote_id: NodeId,
        request: proto::request::Session,
        mut rc: mpsc::Receiver<(RequestId, proto::request::Session)>,
    ) -> SessionResult<Arc<DirectSession>> {
        let config = self.config.clone();

        log::info!("Node [{remote_id}] ({with}) tries to establish p2p session.");

        self.guarded
            .transition_incoming(remote_id, InitState::Initializing)
            .await?;

        let tmp_session = self.temporary_session(with).await;

        let (packet, raw_challenge) = challenge::prepare_challenge_response();
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        tmp_session.send(challenge).await.map_err(|_| {
            ProtocolError::SendFailure(RequestError::Generic(
                "Failed to send challenge".to_string(),
            ))
        })?;

        self.guarded
            .transition_incoming(remote_id, InitState::ChallengeReceived)
            .await?;

        log::debug!("Challenge sent to Node at address: {with}");

        // Compute challenge in different thread to avoid blocking runtime.
        let challenge_handle = match request.challenge_req {
            Some(request) => self.solve_challenge(request).await,
            None => futures::future::ok(proto::ChallengeResponse::default()).boxed_local(),
        };

        if let Some((request_id, session)) = rc.next().await {
            log::debug!("Got challenge response from Node at address: {with}");

            // Validate the challenge before we start solving it ourselves.
            // This way we avoid DDoS.
            let (node_id, identities) =
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    config.challenge_difficulty,
                    session.challenge_resp,
                    None,
                )
                .map_err(|e| ProtocolError::InvalidChallenge(e.to_string()))?;

            log::debug!("Challenge from Node: [{node_id}], address: {with} verified.");

            self.guarded
                .transition_incoming(remote_id, InitState::ChallengeVerified)
                .await?;

            let challenge = challenge_handle
                .await
                .map_err(|e| SessionError::Internal(e.to_string()))?;

            self.guarded
                .transition_incoming(remote_id, InitState::ChallengeSolved)
                .await?;

            let packet = proto::response::Session {
                challenge_resp: Some(challenge),
                ..Default::default()
            };

            self.guarded
                .transition_incoming(remote_id, InitState::ChallengeResponseSent)
                .await?;

            // Register incoming session before we send final response.
            // This way we will avoid race conditions, in case remote Node will attempt
            // to immediately send us Forward packet.
            let session = self
                .layer
                .register_session(with, session_id, node_id, identities)
                .await
                .map_err(|e| {
                    SessionError::Internal(format!("Failed to register session. Error: {e}"))
                })?;

            self.guarded
                .transition_incoming(remote_id, InitState::SessionRegistered)
                .await?;

            // Temporarily pause forwarding from this node
            session.raw.forward_pause.enable();
            session
                .raw
                .send(proto::Packet::response(
                    request_id,
                    session_id.to_vec(),
                    proto::StatusCode::Ok,
                    packet,
                ))
                .await
                .map_err(|_| RequestError::Generic("Sending challenge response.".to_string()))
                .map_err(ProtocolError::from)?;

            // Await for forwarding to be resumed
            if let Some(resumed) = session.raw.forward_pause.next() {
                log::debug!(
                    "Session {session_id} (node = {node_id}) is awaiting a ResumeForwarding message"
                );
                resumed.await;
            }

            log::info!(
                "Incoming P2P session {session_id} with Node: [{node_id}], address: {with} established."
            );

            // Send ping to measure response time.
            let session_ = session.clone();
            tokio::task::spawn_local(async move {
                session_.raw.ping().await.ok();
            });

            return Ok(session);
        }

        Err(SessionError::Timeout(
            "Gave up, waiting for Session messages from peer.".to_string(),
        ))
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        let mut state = self.state.lock().unwrap();
        state.incoming_sessions.remove(session_id);
    }

    pub async fn shutdown(&self) {
        let handles = {
            let mut state = self.state.lock().unwrap();
            state.incoming_sessions.clear();
            std::mem::take(&mut state.handles)
        };

        for handle in handles {
            handle.abort();
        }
    }

    async fn solve_challenge<'a>(
        &self,
        request: proto::ChallengeRequest,
    ) -> LocalBoxFuture<'a, anyhow::Result<proto::ChallengeResponse>> {
        let crypto_vec = match self.list_crypto().await {
            Ok(crypto) => crypto,
            Err(e) => return Box::pin(futures::future::err(e)),
        };

        let limit = self.simultaneous_challenges.clone();

        // Compute challenge in different thread to avoid blocking runtime.
        // Note: computing starts here, not after awaiting.
        async move {
            // Challenge will be solved in thread, where blocking operations are allowed.
            // As tokio documentation for `spawn_blocking` states, number of blocking threads
            // can be very high, so for CPU consuming task, we need to manage number of threads ourselves.
            let _permit = limit.acquire().await?;
            challenge::solve::<ChallengeDigest, _>(
                request.challenge,
                request.difficulty,
                crypto_vec,
            )
            .await
        }
        .boxed_local()
    }

    async fn prepare_challenge_request(
        &self,
        challenge: bool,
    ) -> SessionResult<(proto::request::Session, RawChallenge)> {
        let (mut request, raw_challenge) = challenge::prepare_challenge_request();

        let crypto = self
            .list_crypto()
            .await
            .map_err(|e| SessionError::Internal(format!("Failed to query identities: {e}")))?;

        let mut identities = vec![];
        for id in crypto {
            let pub_key = id
                .public_key()
                .await
                .map_err(|e| SessionError::Internal(e.to_string()))?;
            identities.push(proto::Identity {
                node_id: pub_key.address().to_vec(),
                public_key: pub_key.bytes().to_vec(),
            })
        }

        request.identities = identities;

        if !challenge {
            request.challenge_req = None;
        }

        Ok((request, raw_challenge))
    }

    async fn list_crypto(&self) -> anyhow::Result<Vec<Rc<dyn Crypto>>> {
        let mut crypto_vec = match self.config.crypto.get(self.config.node_id).await {
            Ok(crypto) => vec![crypto],
            Err(e) => bail!(e),
        };
        let aliases = match self.config.crypto.aliases().await {
            Ok(aliases) => aliases,
            Err(e) => bail!(e),
        };
        for alias in aliases {
            match self.config.crypto.get(alias).await {
                Ok(crypto) => crypto_vec.push(crypto),
                _ => log::debug!("Unable to retrieve Crypto instance for id [{alias}]"),
            }
        }
        Ok(crypto_vec)
    }
}
