use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable};
use futures::{FutureExt, SinkExt, StreamExt};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use ya_relay_core::challenge::{self, ChallengeDigest, CHALLENGE_DIFFICULTY};
use ya_relay_core::error::{BadRequest, InternalError, ServerResult, Unauthorized};
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::RequestId;

use crate::dispatch::Dispatcher;
use crate::session::Session;
use crate::session_manager::SessionManager;

#[derive(Clone)]
pub(crate) struct StartingSessions {
    state: Arc<Mutex<StartingSessionsState>>,

    layer: SessionManager,
    sink: OutStream,
}

#[derive(Default)]
struct StartingSessionsState {
    /// Initialization of session started by other peers.
    incoming_sessions: HashMap<SessionId, mpsc::Sender<(RequestId, proto::request::Session)>>,
    /// Temporary sessions stored during initialization period.
    /// After session is established, new struct in SessionManager is created
    /// and this one is removed.
    tmp_sessions: HashMap<SocketAddr, Arc<Session>>,
    tmp_node_ids: HashMap<NodeId, oneshot::Sender<()>>,

    /// Collection of background tasks that must be stopped on shutdown.
    handles: Vec<AbortHandle>,
}

impl StartingSessions {
    pub fn new(layer: SessionManager, sink: OutStream) -> StartingSessions {
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
        let session_id = SessionId::generate();
        let (sender, receiver) = mpsc::channel(1);

        log::info!("Node ({}) tries to establish p2p session.", with);

        let (abort_handle, abort_registration) = AbortHandle::new_pair();

        // TODO: Add timeout for session initialization.
        let myself = self.clone();
        let init_future = Abortable::new(
            async move {
                if let Err(e) = self
                    .clone()
                    .init_session_handler(with, request_id, session_id, request, receiver)
                    .await
                {
                    log::warn!(
                        "Error initializing session {} with node {}. Error: {}",
                        session_id,
                        with,
                        e
                    );

                    // Establishing session failed.
                    self.layer
                        .error_response(request_id, session_id.to_vec(), &with, e)
                        .await;
                    self.cleanup_initialization(&session_id).await;
                }
            },
            abort_registration,
        );

        {
            let mut state = myself.state.lock().unwrap();
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
        request: proto::request::Session,
        mut rc: mpsc::Receiver<(RequestId, proto::request::Session)>,
    ) -> ServerResult<()> {
        let (packet, raw_challenge) = challenge::prepare_challenge_response();
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
                .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?;

            log::debug!(
                "Challenge from Node: [{}], address: {} verified.",
                node_id,
                with
            );

            if let Some(sender) = { self.state.lock().unwrap().tmp_node_ids.remove(&node_id) } {
                // Try to fire the event, ignore failures
                let _ = sender.send(());
            }

            let packet = proto::response::Session {
                challenge_resp: Some(
                    challenge_handle
                        .await
                        .map_err(|e| InternalError::Generic(e.to_string()))?,
                ),
                ..Default::default()
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
                .add_incoming_session(with, session_id, node_id, identities)
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

    pub fn register_waiting_for_node(&self, node_id: NodeId) -> StartRegistration {
        let (reg, tx) = StartRegistration::with(self.state.clone(), node_id);
        self.state.lock().unwrap().tmp_node_ids.insert(node_id, tx);
        reg
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

    pub async fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();

        for handle in &state.handles {
            handle.abort();
        }

        state.incoming_sessions.clear();
        state.tmp_sessions.clear();
    }
}

pub struct StartRegistration {
    node_id: NodeId,
    state: Arc<Mutex<StartingSessionsState>>,
    rx: Option<oneshot::Receiver<()>>,
}

impl StartRegistration {
    fn with(
        state: Arc<Mutex<StartingSessionsState>>,
        node_id: NodeId,
    ) -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();
        let reg = Self {
            node_id,
            state,
            rx: Some(rx),
        };
        (reg, tx)
    }

    #[inline]
    pub fn rx(&mut self) -> anyhow::Result<oneshot::Receiver<()>> {
        self.rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("Registration receiver already taken"))
    }
}

impl Drop for StartRegistration {
    fn drop(&mut self) {
        if let Some(tx) = {
            self.state
                .lock()
                .unwrap()
                .tmp_node_ids
                .remove(&self.node_id)
        } {
            // Try to fire the event, ignore failures
            let _ = tx.send(());
        }
    }
}
