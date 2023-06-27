use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::time::timeout;

use ya_relay_core::challenge::{self, ChallengeDigest, CHALLENGE_DIFFICULTY};
use ya_relay_core::error::{InternalError, ServerResult, Unauthorized};
use ya_relay_core::server_session::SessionId;
use ya_relay_core::session::{Session, SessionError, SessionResult};
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::RequestId;

use crate::session_guard::GuardedSessions;
use crate::session_manager::SessionManager;

#[derive(Clone)]
pub(crate) struct StartingSessions {
    state: Arc<Mutex<StartingSessionsState>>,

    guarded: GuardedSessions,
    layer: SessionManager,
    sink: OutStream,
}

#[derive(Default)]
struct StartingSessionsState {
    /// Initialization of session started by other peers.
    incoming_sessions: HashMap<SessionId, mpsc::Sender<(RequestId, proto::request::Session)>>,

    /// Collection of background tasks that must be stopped on shutdown.
    handles: Vec<AbortHandle>,
}

impl StartingSessions {
    pub fn new(
        layer: SessionManager,
        guarded: GuardedSessions,
        sink: OutStream,
    ) -> StartingSessions {
        StartingSessions {
            state: Arc::new(Mutex::new(StartingSessionsState::default())),
            sink,
            layer,
            guarded,
        }
    }

    /// Creates temporary session used only during initialization.
    /// Only one session will be created for one target Node address.
    /// If temporary session was already created, this function will wait for initialization finish.
    pub async fn temporary_session(&mut self, addr: SocketAddr) -> Arc<Session> {
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
        .map_err(|e| log::warn!("{}", e))
        .ok();
    }

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
            timeout(self.layer.config.incoming_session_timeout, async move {
                // In case of ReverseConnection, we are awaiting incoming connection from
                // other party
                if this.guarded.try_access_unguarded(remote_id).await {
                    log::debug!(
                        "Node [{}] enters unguarded session initialization.",
                        remote_id
                    );

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
            Ok(Err(_timeout)) => Err(SessionError::Drop("Timeout".to_string())),
            Err(_aborted) => Err(SessionError::Retry("Aborted".to_string())),
        })
        .or_else(move |result| async move {
            if let Some(session) = this1.guarded.get_temporary_session(&with).await {
                session.disconnect().await.ok();
            }

            log::warn!(
                "Error initializing session {} with node [{}] ({}). Error: {}",
                session_id,
                remote_id,
                with,
                result
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
    ) -> SessionResult<Arc<Session>> {
        // If we are connected and other side isn't, than maybe we should let him establish new session.
        if self.layer.assert_not_connected(&[with]).await.is_err() {
            if let Some(session) = self.layer.get_session(with).await {
                log::info!("Node [{remote_id}] ({with}) trying to start new session, despite it is already established. Closing previous session.");
                self.layer.close_session(session).await.ok();
            }
        }

        log::info!(
            "Node [{}] ({}) tries to establish p2p session.",
            remote_id,
            with
        );

        self.guarded.notify_first_message(remote_id).await;

        let tmp_session = self.temporary_session(with).await;

        let (packet, raw_challenge) =
            challenge::prepare_challenge_response(self.layer.config.challenge_difficulty);
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        tmp_session
            .send(challenge)
            .await
            .map_err(|_| SessionError::Drop("Failed to send challenge".to_string()))?;

        log::debug!("Challenge sent to Node at address: {}", with);

        // Compute challenge in different thread to avoid blocking runtime.
        let challenge_handle = match request.challenge_req {
            Some(request) => self.layer.solve_challenge(request).await,
            None => futures::future::ok(proto::ChallengeResponse::default()).boxed_local(),
        };

        if let Some((request_id, session)) = rc.next().await {
            log::debug!("Got challenge response from Node at address: {}", with);

            // Validate the challenge
            let (node_id, identities) =
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    CHALLENGE_DIFFICULTY,
                    session.challenge_resp,
                    None,
                )
                .map_err(|e| SessionError::Drop(format!("Invalid challenge: {}", e)))?;

            log::debug!(
                "Challenge from Node: [{}], address: {} verified.",
                node_id,
                with
            );

            let packet = proto::response::Session {
                challenge_resp: Some(
                    challenge_handle
                        .await
                        .map_err(|e| SessionError::Drop(e.to_string()))?,
                ),
                ..Default::default()
            };

            // Register incoming session before we send final response.
            // This way we will avoid race conditions, in case remote Node will attempt
            // to immediately send us Forward packet.
            let session = self
                .layer
                .add_incoming_session(with, session_id, node_id, identities)
                .await
                .map_err(|e| {
                    SessionError::Drop(format!("Failed to add incoming session. {}", e))
                })?;

            // Temporarily pause forwarding from this node
            session.forward_pause.enable();
            session
                .send(proto::Packet::response(
                    request_id,
                    session_id.to_vec(),
                    proto::StatusCode::Ok,
                    packet,
                ))
                .await
                .map_err(|_| {
                    SessionError::Drop("Failed to send challenge response.".to_string())
                })?;
            // Await for forwarding to be resumed
            if let Some(resumed) = session.forward_pause.next() {
                log::debug!(
                    "Session {session_id} (node = {node_id}) is awaiting a ResumeForwarding message"
                );
                resumed.await;
            }

            log::info!(
                "Incoming P2P session {session_id} with Node: [{node_id}], address: {with} established."
            );

            // Send ping to measure response time.
            let session_cp = session.clone();
            tokio::task::spawn_local(async move {
                session_cp.ping().await.ok();
            });

            return Ok(session);
        }

        Err(SessionError::Drop(
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
}
