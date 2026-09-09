use anyhow::bail;
use futures::future::{AbortHandle, Abortable, LocalBoxFuture};
use futures::{FutureExt, SinkExt};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;

use ya_relay_core::challenge::{self, ChallengeDigest, RawChallenge};
use ya_relay_core::crypto::Crypto;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_proto::proto;
use ya_relay_proto::proto::RequestId;

use super::network_view::SessionPermit;
use crate::client::ClientConfig;
use crate::direct_session::DirectSession;
use crate::encryption;
use crate::error::{ProtocolError, RequestError, SessionError, SessionInitError, SessionResult};
use crate::raw_session::RawSession;
use crate::session::session_state::InitState;
use crate::session::session_traits::{SessionRegistration, SessionRegistrationInfo};

// Admission is checked before spawning a handshake task or consulting the node registry.
const MAX_INCOMING_HANDSHAKES: usize = 256;

#[cfg(test)]
mod tests;

struct IncomingSession {
    remote: SocketAddr,
    sender: mpsc::Sender<(RequestId, proto::request::Session)>,
}

struct TemporarySession {
    raw: Arc<RawSession>,
    users: usize,
}

#[derive(Clone)]
pub struct SessionInitializer {
    state: Arc<Mutex<SessionInitializerState>>,
    layer: Arc<Box<dyn SessionRegistration>>,

    simultaneous_challenges: Arc<Semaphore>,
    config: Arc<ClientConfig>,
    sink: OutStream,
}

#[derive(Default)]
pub struct SessionInitializerState {
    /// Initialization of session started by other peers.
    incoming_sessions: HashMap<SessionId, IncomingSession>,
    admitted: HashMap<SocketAddr, u64>,
    next_admission: u64,

    /// Temporary sessions stored during initialization period.
    /// After session is established, new struct in `SessionLayer` is created
    /// and this one is removed.
    tmp_sessions: HashMap<SocketAddr, TemporarySession>,

    /// Collection of background tasks that must be stopped on shutdown.
    handles: HashMap<SessionId, AbortHandle>,
}

pub(crate) struct IncomingAdmission {
    state: Arc<Mutex<SessionInitializerState>>,
    remote: SocketAddr,
    token: u64,
}

impl Drop for IncomingAdmission {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.admitted.get(&self.remote) == Some(&self.token) {
            state.admitted.remove(&self.remote);
        }
    }
}

struct IncomingSessionGuard {
    state: Arc<Mutex<SessionInitializerState>>,
    id: SessionId,
}

impl Drop for IncomingSessionGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.incoming_sessions.remove(&self.id);
        state.handles.remove(&self.id);
    }
}

/// Cancelling one attempt must not remove a temporary session created later.
struct TemporarySessionGuard {
    state: Arc<Mutex<SessionInitializerState>>,
    session: Arc<RawSession>,
}

impl Drop for TemporarySessionGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.tmp_sessions.get_mut(&self.session.remote) {
            if Arc::ptr_eq(&entry.raw, &self.session) {
                entry.users -= 1;
                if entry.users == 0 {
                    state.tmp_sessions.remove(&self.session.remote);
                }
            }
        }
    }
}

impl SessionInitializer {
    pub(crate) fn admit_incoming(&self, remote: SocketAddr) -> Option<IncomingAdmission> {
        let mut state = self.state.lock().unwrap();
        if state.admitted.contains_key(&remote) || state.admitted.len() >= MAX_INCOMING_HANDSHAKES {
            return None;
        }
        let token = state.next_admission.checked_add(1)?;
        state.next_admission = token;
        state.admitted.insert(remote, token);
        Some(IncomingAdmission {
            state: self.state.clone(),
            remote,
            token,
        })
    }

    pub(crate) fn incoming_timeout(&self) -> std::time::Duration {
        self.config.incoming_session_timeout
    }

    pub(crate) fn new(
        config: Arc<ClientConfig>,
        layer: impl SessionRegistration + 'static,
        sink: OutStream,
    ) -> SessionInitializer {
        // We don't want to overwhelm CPU with challenge solving operations.
        // Leave at least 2 threads for yagna to work.
        let max_heavy_threads = std::cmp::max(num_cpus::get(), 3) - 2;

        SessionInitializer {
            state: Arc::new(Mutex::new(SessionInitializerState::default())),
            sink,
            layer: Arc::new(Box::new(layer)),
            config,
            simultaneous_challenges: Arc::new(Semaphore::new(max_heavy_threads)),
        }
    }

    /// Test transport helper. Production attempts must use a lifetime-bound lease.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn temporary_session(&self, addr: &SocketAddr) -> Arc<RawSession> {
        let mut state = self.state.lock().unwrap();
        state
            .tmp_sessions
            .entry(*addr)
            .or_insert_with(|| TemporarySession {
                raw: RawSession::new(*addr, SessionId::generate(), self.sink.clone()),
                users: 0,
            })
            .raw
            .clone()
    }

    fn lease_temporary_session(&self, addr: SocketAddr) -> TemporarySessionGuard {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .tmp_sessions
            .entry(addr)
            .or_insert_with(|| TemporarySession {
                raw: RawSession::new(addr, SessionId::generate(), self.sink.clone()),
                users: 0,
            });
        entry.users += 1;
        TemporarySessionGuard {
            state: self.state.clone(),
            session: entry.raw.clone(),
        }
    }

    /// Different from `temporary_session` because it doesn't create new session.
    pub fn get_temporary_session(&self, addr: &SocketAddr) -> Option<Arc<RawSession>> {
        let state = self.state.lock().unwrap();
        state.tmp_sessions.get(addr).map(|entry| entry.raw.clone())
    }

    /// External layer is responsible for acquiring `SessionPermit` to make sure,
    /// that we are not processing 2 session initializations at the same time.
    ///
    /// Rationale: In most cases synchronizing everything internally would be preferred
    /// over moving this responsibility somewhere else. But the reason for this design
    /// is to decouple as many components as possible, to make them testable separately.
    /// We have many methods of initialization, moreover initiative can be on both sides
    /// of the connection. To synchronize everything internally, we would have to create
    /// single struct protecting all the logic. Code would become complicated and hard
    /// to understand.
    async fn init_session(
        &self,
        addr: SocketAddr,
        permit: &SessionPermit,
        challenge: bool,
    ) -> SessionResult<Arc<DirectSession>> {
        let config = self.config.clone();
        let this_id = config.node_id;
        let guard = permit.registry.clone();
        let node_id = guard.id;

        guard.transition_outgoing(InitState::Initializing).await?;

        let _temporary = self.lease_temporary_session(addr);
        let tmp_session = &_temporary.session;

        log::debug!("[{this_id}] initializing session with [{node_id}] ({addr})");

        let (request, raw_challenge) = self.prepare_session_request(challenge).await?;
        let response = tmp_session
            .request::<proto::response::Session>(
                request.into(),
                vec![],
                self.config.session_request_timeout,
            )
            .await?;

        guard
            .transition_outgoing(InitState::ChallengeHandshake)
            .await?;

        let session_id = SessionId::try_from(response.session_id.as_slice())
            .map_err(|e| ProtocolError::InvalidResponse(e.to_string()))?;
        let challenge_req = response.packet.challenge_req.ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Expected ChallengeRequest while initializing session with {addr}".to_string(),
            )
        })?;
        let challenge_handle = self.solve_challenge(challenge_req).await;

        log::trace!("Solving challenge while establishing session with: [{node_id}] ({addr})");

        // with the current ECDSA scheme the public key
        // can be recovered from challenge signature
        let packet = proto::request::Session {
            challenge_resp: Some(
                challenge_handle
                    .await
                    .map_err(|e| SessionError::Internal(e.to_string()))?,
            ),
            supported_encryptions: encryption::supported_encryptions(),
            ..Default::default()
        };

        let response = tmp_session
            .request::<proto::response::Session>(
                packet.into(),
                session_id.to_vec(),
                config.challenge_request_timeout,
            )
            .await?;

        log::trace!("Challenge response sent to: [{node_id}] ({addr})");
        guard
            .transition_outgoing(InitState::HandshakeResponse)
            .await?;

        if session_id != &response.session_id[..] {
            let _ = tmp_session.disconnect().await;
            return Err(ProtocolError::InvalidResponse(format!(
                "Session id mismatch: {} (expected) vs {:?} (response)",
                session_id, response.session_id,
            ))
            .into());
        }

        let recovered = if challenge {
            log::trace!("Validating challenge from: [{node_id}] ({addr})");

            challenge::recover_identities_from_challenge::<ChallengeDigest>(
                &raw_challenge,
                config.challenge_difficulty,
                response.packet.challenge_resp,
                Some(node_id),
            )
        } else {
            Ok(Default::default())
        };
        let (remote_id, identities, session_key) = match recovered {
            Ok(tuple) => tuple,
            Err(e) => {
                let _ = tmp_session.disconnect().await;
                return Err(ProtocolError::InvalidChallenge(format!("{e}")).into());
            }
        };

        guard
            .transition_outgoing(InitState::ChallengeVerified)
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
            .register_session(
                SessionRegistrationInfo {
                    addr,
                    id: session_id,
                    node_id: remote_id,
                    identities,
                    supported_encryptions: response.packet.supported_encryptions,
                    session_key,
                },
                &guard,
            )
            .await
            .map_err(|e| {
                SessionError::Internal(format!("Failed to register session. Error: {e}"))
            })?;

        guard
            .transition_outgoing(InitState::SessionRegistered)
            .await?;

        session
            .raw
            .send(proto::Packet::control(
                session.raw.id.to_vec(),
                proto::control::ResumeForwarding::default(),
            ))
            .await
            .map_err(RequestError::from)?;

        guard.transition_outgoing(InitState::Ready).await?;

        log::trace!("[{this_id}] session {session_id} established with: [{node_id}] ({addr})");

        // Send ping to measure response time.
        // Otherwise we will have very big response time, when querying this information
        // directly after establishing session.
        let session_ = session.clone();
        tokio::task::spawn_local(async move {
            session_.raw.ping().await.ok();
        });

        Ok(session)
    }

    /// External layer is responsible for acquiring `SessionPermit` to make sure,
    /// that we are not processing 2 session initializations at the same time.
    ///
    /// See `SessionProtocol::init_session` for rationale behind this decision.
    pub async fn init_p2p_session(
        &self,
        addr: SocketAddr,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionInitError> {
        let node_id = permit.registry.id;

        log::info!("Initializing p2p session with Node: [{node_id}], address: {addr}");
        let session = self
            .init_session(addr, permit, true)
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

    /// External layer is responsible for acquiring `SessionPermit` to make sure,
    /// that we are not processing 2 session initializations at the same time.
    ///
    /// See `SessionProtocol::init_session` for rationale behind this decision.
    pub(crate) async fn init_server_session(
        &self,
        addr: SocketAddr,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionInitError> {
        log::info!("Initializing session with NET relay server at: {addr}");

        let session = self
            .init_session(addr, permit, false)
            .await
            .map_err(|e| SessionInitError::Relay(addr, e))?;

        log::info!(
            "Established session {} with NET relay server ({addr})",
            session.raw.id,
        );

        Ok(session)
    }

    /// External layer is responsible for acquiring `SessionPermit` to make sure,
    /// that we are not processing 2 session initializations at the same time.
    ///
    /// See `SessionProtocol::init_session` for rationale behind this decision.
    pub async fn new_session(
        self,
        request_id: RequestId,
        with: SocketAddr,
        permit: &SessionPermit,
        request: proto::request::Session,
    ) -> Result<Arc<DirectSession>, SessionError> {
        let session_id = SessionId::generate();
        let remote_id = permit.registry.id;
        let (sender, receiver) = mpsc::channel(1);

        // TODO: We don't need abort-ability on this level, because we have this functionality
        //       on external layers. Remove abort handling from here
        let (abort_handle, abort_registration) = AbortHandle::new_pair();

        {
            let mut state = self.state.lock().unwrap();
            if state.incoming_sessions.len() >= MAX_INCOMING_HANDSHAKES
                || state
                    .incoming_sessions
                    .values()
                    .any(|entry| entry.remote == with)
            {
                return Err(SessionError::Internal(
                    "Incoming handshake slot unavailable".into(),
                ));
            }
            state.incoming_sessions.insert(
                session_id,
                IncomingSession {
                    remote: with,
                    sender,
                },
            );
            state.handles.insert(session_id, abort_handle);
        }

        let _incoming = IncomingSessionGuard {
            state: self.state.clone(),
            id: session_id,
        };
        let _temporary = self.lease_temporary_session(with);

        let this = self.clone();
        let session = Abortable::new(
            timeout(self.config.incoming_session_timeout, async move {
                this.init_session_handler(
                    with, request_id, session_id, permit, request, receiver,
                ).await
            }),
            abort_registration,
        )
        .map(move |result| match result {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_timeout)) => Err(SessionError::Timeout("Incoming handshake deadline exceeded".into())),
            Err(_aborted) => Err(SessionError::Internal("Aborted".to_string())),
        })
        .await.map_err(move |result| {
            log::warn!(
                "Error initializing session {session_id} with node [{remote_id}] ({with}). Error: {result}"
            );

            result
        })?;

        Ok(session)
    }

    pub(crate) fn try_continue_session(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) -> Result<(), SessionError> {
        let state = self.state.lock().unwrap();
        let sender = match state.incoming_sessions.get(&session_id) {
            Some(entry) if entry.remote == from => &entry.sender,
            Some(_) => return Err(SessionError::Internal("Handshake endpoint mismatch".into())),
            None => return Err(ProtocolError::SessionNotFound(session_id).into()),
        };

        sender
            .try_send((request_id, request))
            .map_err(|_| SessionError::Internal("Handshake queue unavailable".to_string()))
    }

    /// External layer is responsible for acquiring `SessionPermit` to make sure,
    /// that we are not processing 2 session initializations at the same time.
    ///
    /// See `SessionProtocol::init_session` for rationale behind this decision.
    async fn init_session_handler(
        self,
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        permit: &SessionPermit,
        request: proto::request::Session,
        mut rc: mpsc::Receiver<(RequestId, proto::request::Session)>,
    ) -> SessionResult<Arc<DirectSession>> {
        let config = self.config.clone();
        let remote_id = permit.registry.id;
        let guard = permit.registry.clone();

        log::info!("Node [{remote_id}] ({with}) tries to establish p2p session.");

        guard.transition_incoming(InitState::Initializing).await?;

        let tmp_session = self.get_temporary_session(&with).ok_or_else(|| {
            SessionError::Internal("Incoming handshake lease was released".into())
        })?;

        let (packet, raw_challenge) =
            challenge::prepare_challenge_response(config.challenge_difficulty);
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        tmp_session
            .send(challenge)
            .await
            .map_err(|_| RequestError::Generic("Failed to send challenge".to_string()))?;

        guard
            .transition_incoming(InitState::ChallengeHandshake)
            .await?;

        log::debug!("Challenge sent to Node [{remote_id}] at address: {with}");

        // Compute challenge in different thread to avoid blocking runtime.
        let challenge_handle = match request.challenge_req {
            Some(request) => self.solve_challenge(request).await,
            None => futures::future::ok(proto::ChallengeResponse::default()).boxed_local(),
        };

        if let Some((request_id, session)) = rc.recv().await {
            log::debug!("Got challenge response from Node [{remote_id}] at address: {with}");

            guard
                .transition_incoming(InitState::HandshakeResponse)
                .await?;

            // Validate the challenge before we start solving it ourselves.
            // This way we avoid DDoS.
            let (node_id, identities, session_key) =
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    config.challenge_difficulty,
                    session.challenge_resp,
                    Some(remote_id),
                )
                .map_err(|e| ProtocolError::InvalidChallenge(e.to_string()))?;

            log::debug!("Challenge from Node: [{node_id}], address: {with} verified.");

            let challenge = challenge_handle
                .await
                .map_err(|e| SessionError::Internal(e.to_string()))?;

            guard
                .transition_incoming(InitState::ChallengeVerified)
                .await?;

            let packet = proto::response::Session {
                challenge_resp: Some(challenge),
                supported_encryptions: encryption::supported_encryptions(),
                ..Default::default()
            };

            // Register incoming session before we send final response.
            // This way we will avoid race conditions, in case remote Node will attempt
            // to immediately send us Forward packet.
            let session = self
                .layer
                .register_session(
                    SessionRegistrationInfo {
                        addr: with,
                        id: session_id,
                        node_id,
                        identities,
                        supported_encryptions: session.supported_encryptions,
                        session_key,
                    },
                    &guard,
                )
                .await
                .map_err(|e| {
                    SessionError::Internal(format!("Failed to register session. Error: {e}"))
                })?;

            guard
                .transition_incoming(InitState::SessionRegistered)
                .await?;

            // Temporarily pause forwarding from this node
            session.pause_forwarding().await;
            session
                .raw
                .send(proto::Packet::response(
                    request_id,
                    session_id.to_vec(),
                    proto::StatusCode::Ok,
                    packet,
                ))
                .await
                .map_err(|_| RequestError::Generic("Sending challenge response.".to_string()))?;

            // Await for forwarding to be resumed
            session.wait_for_resume().await;
            guard.transition_incoming(InitState::Ready).await?;

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
            "Gave up waiting for Session messages from peer.".to_string(),
        ))
    }

    pub async fn shutdown(&mut self) {
        let handles = {
            let mut state = self.state.lock().unwrap();
            state.incoming_sessions.clear();
            state.tmp_sessions.clear();
            std::mem::take(&mut state.handles)
        };

        for (_, handle) in handles {
            handle.abort();
        }

        self.sink.close().await.ok();
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
        let session_public_key = self.config.session_crypto.pub_key();

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
                session_public_key,
            )
            .await
        }
        .boxed_local()
    }

    async fn prepare_session_request(
        &self,
        challenge: bool,
    ) -> SessionResult<(proto::request::Session, RawChallenge)> {
        let (mut request, raw_challenge) =
            challenge::prepare_challenge_request(self.config.challenge_difficulty);
        request.client_version = env!("CARGO_PKG_VERSION").to_owned();

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
