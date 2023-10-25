use super::*;
use crate::server::session::metric::SessionMetric;
use crate::AddrStatus;
use std::sync::Arc;

mod metric {
    use metrics::{recorder, Counter, Key};
    const SESSION_EST_START: Key = Key::from_static_name("ya-relay.session.establish.start");
    const SESSION_EST_ERROR: Key = Key::from_static_name("ya-relay.session.establish.error");
    const SESSION_EST_CHALLENGE_SENT: Key =
        Key::from_static_name("ya-relay.session.establish.challenge.sent");
    const SESSION_EST_CHALLENGE_VALID: Key =
        Key::from_static_name("ya-relay.session.establish.challenge.valid");

    pub(super) struct SessionMetric {
        pub start: Counter,
        pub error: Counter,
        pub challenge_sent: Counter,
        pub challenge_valid: Counter,
    }

    impl Default for SessionMetric {
        fn default() -> Self {
            let start = recorder().register_counter(&SESSION_EST_START);
            let error = recorder().register_counter(&SESSION_EST_ERROR);
            let challenge_sent = recorder().register_counter(&SESSION_EST_CHALLENGE_SENT);
            let challenge_valid = recorder().register_counter(&SESSION_EST_CHALLENGE_VALID);
            Self {
                start,
                error,
                challenge_sent,
                challenge_valid,
            }
        }
    }
}

pub struct SessionHandler {
    difficulty: u64,
    session_manager: Arc<SessionManager>,
    metrics: SessionMetric,
    challage_send_ack: CompletionHandler,
    challage_valid_ack: CompletionHandler,
}

impl SessionHandler {
    pub fn new(difficulty: u64, session_manager: &Arc<SessionManager>) -> Self {
        let session_manager = Arc::clone(session_manager);
        let metrics = SessionMetric::default();
        let challage_send_ack = counter_ack(&metrics.challenge_sent, &metrics.error);
        let challage_valid_ack = counter_ack(&metrics.challenge_valid, &metrics.error);
        Self {
            difficulty,
            session_manager,
            metrics,
            challage_send_ack,
            challage_valid_ack,
        }
    }

    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: Option<SessionId>,
        session: &request::Session,
    ) -> Option<(CompletionHandler, Packet)> {
        if let Some(session_id) = session_id {
            log::debug!(target: "request::session", "[{src}] got challenge response session_id={session_id}, request_id={request_id}");
            match self.session_manager.with_session(&session_id, |session| {
                clock.touch(&session.ts);

                if let &SessionState::Pending {
                    challenge,
                    difficulty,
                } = &*session.state.lock()
                {
                    (session.peer == src, Some((challenge, difficulty)))
                } else {
                    (session.peer == src, None)
                }
            }) {
                None | Some((false, _)) => {
                    // session does not exists or it is from different address.
                    log::warn!(target: "request::session", "[{src}] Challenge response to non existend session: {session_id}");
                    self.metrics.error.increment(1);
                    return None;
                }
                Some((true, None)) => {
                    // session is already established we response with ack.
                    log::debug!(target: "request::session", "[{src}] retry session est ack: {session_id}");
                    Some((
                        noop_ack(),
                        Packet {
                            session_id: session_id.to_vec(),
                            kind: Some(packet::Kind::Response(Response {
                                code: StatusCode::Ok.into(),
                                request_id,
                                kind: Some(response::Kind::Session(response::Session::default())),
                            })),
                        },
                    ))
                }
                Some((true, Some((challenge, difficulty)))) => {
                    match challenge::recover_identities_from_challenge::<ChallengeDigest>(
                        &challenge,
                        difficulty,
                        session.challenge_resp.clone(),
                        None,
                    ) {
                        Err(e) => {
                            self.metrics.error.increment(1);
                            log::warn!(target: "request::session", "[{src}] challenge verification failed for session_id={session_id}: {e:?}");
                            Some((
                                noop_ack(),
                                Packet {
                                    session_id: session_id.to_vec(),
                                    kind: Some(packet::Kind::Response(Response {
                                        code: StatusCode::BadRequest.into(),
                                        request_id,
                                        kind: Some(response::Kind::Session(Default::default())),
                                    })),
                                },
                            ))
                        }
                        Ok((node_id, keys)) => {
                            self.session_manager.with_session(&session_id, |session| {
                                *session.state.lock() = SessionState::Est {
                                    node_id,
                                    keys,
                                    addr_status: AddrStatus::Unknown,
                                }
                            });
                            log::debug!(target: "request::session", "[{src}] established session with {node_id}");
                            Some((
                                self.challage_valid_ack.clone(),
                                Packet::response(
                                    request_id,
                                    session_id.to_vec(),
                                    StatusCode::Ok,
                                    response::Kind::Session(Default::default()),
                                ),
                            ))
                        }
                    }
                }
            }
        } else {
            let (session, challenge) = challenge::prepare_challenge_response(self.difficulty);
            let session_ref =
                self.session_manager
                    .new_session(clock, src, challenge, self.difficulty);
            self.metrics.start.increment(1);
            log::debug!(target: "request::session", "[{src}] Starting session {}", session_ref.session_id);
            Some((
                self.challage_send_ack.clone(),
                Packet {
                    session_id: session_ref.session_id.to_vec(),
                    kind: Some(packet::Kind::Response(Response {
                        code: StatusCode::Ok.into(),
                        request_id,
                        kind: Some(response::Kind::Session(session)),
                    })),
                },
            ))
        }
    }
}
