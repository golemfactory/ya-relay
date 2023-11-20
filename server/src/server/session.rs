use std::sync::Arc;
use std::time;

use tiny_keccak::Hasher;

use ya_relay_core::challenge::RawChallenge;

use crate::server::session::metric::SessionMetric;

use super::*;

mod metric {
    use metrics::{recorder, Counter, Key};

    static SESSION_EST_START: Key = Key::from_static_name("ya-relay.session.establish.start");
    static SESSION_EST_ERROR: Key = Key::from_static_name("ya-relay.session.establish.error");
    static SESSION_EST_CHALLENGE_SENT: Key =
        Key::from_static_name("ya-relay.session.establish.challenge.sent");
    static SESSION_EST_CHALLENGE_VALID: Key =
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

#[derive(clap::Args, Clone)]
/// Ip Checker configuration args
#[command(next_help_heading = "Session handler options")]
pub struct SessionHandlerConfig {
    #[arg(long, env, default_value = "16")]
    pub difficulty: u64,
    #[arg(long, env, value_parser = u128_from_hex)]
    pub salt: Option<u128>,
}

fn u128_from_hex(hex_str: &str) -> Result<u128, hex::FromHexError> {
    let bytes: [u8; 16] = hex::FromHex::from_hex(hex_str)?;
    Ok(u128::from_le_bytes(bytes))
}

pub struct SessionHandler {
    difficulty: u64,
    salt: [u8; 16],
    session_manager: Arc<SessionManager>,
    metrics: SessionMetric,
    challenge_send_ack: CompletionHandler,
    challenge_valid_ack: CompletionHandler,
}

impl SessionHandler {
    pub fn new(session_manager: &Arc<SessionManager>, config: &SessionHandlerConfig) -> Self {
        let session_manager = Arc::clone(session_manager);
        let metrics = SessionMetric::default();
        let challenge_send_ack = counter_ack(&metrics.challenge_sent, &metrics.error);
        let challenge_valid_ack = counter_ack(&metrics.challenge_valid, &metrics.error);
        let salt = config
            .salt
            .unwrap_or_else(|| thread_rng().gen())
            .to_ne_bytes();
        let difficulty = config.difficulty;

        Self {
            difficulty,
            salt,
            session_manager,
            metrics,
            challenge_send_ack,
            challenge_valid_ack,
        }
    }

    fn epoch(&self) -> u64 {
        time::UNIX_EPOCH.elapsed().unwrap().as_secs() / 600
    }

    fn session_challenge(&self, session_id: SessionId) -> RawChallenge {
        let mut h = tiny_keccak::Keccak::v256();
        let mut data = [0u8; 32];
        h.update(&session_id.to_array());
        h.update(&self.salt);
        h.finalize(&mut data);
        let raw_challenge: [u8; 16] = data[..16].try_into().unwrap();
        raw_challenge
    }

    fn check_session_id(&self, session_id: SessionId, addr: SocketAddr) -> bool {
        let epoch = self.epoch();
        for n in 0..3 {
            if session_id == self.gen_new_challenge(addr, epoch - n) {
                return true;
            }
        }

        false
    }
    fn gen_new_challenge(&self, addr: SocketAddr, epoch: u64) -> SessionId {
        let mut data = [0u8; 32];
        let difficulty = self.difficulty;

        let mut h = tiny_keccak::Keccak::v256();
        match addr {
            SocketAddr::V4(v4) => {
                h.update(&v4.ip().octets());
                h.update(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                h.update(&v6.ip().octets());
                h.update(&v6.port().to_be_bytes());
            }
        }
        h.update(&self.salt);
        h.update(&difficulty.to_ne_bytes());
        //h.update(&request_id.to_ne_bytes());
        h.update(&epoch.to_ne_bytes());
        h.finalize(&mut data);

        let session_id: [u8; 16] = (&data[..16]).try_into().unwrap();
        /*let request = proto::ChallengeRequest {
            version: "0.0.1".to_string(),
            caps: 0,
            kind: proto::challenge_request::Kind::Sha3512LeadingZeros as i32,
            difficulty,
            challenge: raw_challenge.to_vec(),
        };*/
        session_id.into()
    }

    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: Option<SessionId>,
        req_session: &request::Session,
    ) -> Option<(CompletionHandler, Packet)> {
        if let Some(session_id) = session_id {
            log::debug!(target: "request::session", "[{src}] got challenge response session_id={session_id}, request_id={request_id}");
            match req_session {
                request::Session {
                    challenge_resp: Some(challenge_resp),
                    supported_encryptions,
                    ..
                } => {
                    let challenge = self.session_challenge(session_id);
                    log::info!(
                        "resp session_id={}, request_id={}, challange={}",
                        session_id,
                        request_id,
                        hex::encode(challenge.as_slice())
                    );

                    let (node_id, keys) = match challenge::recover_identities_from_challenge::<
                        ChallengeDigest,
                    >(
                        &challenge,
                        self.difficulty,
                        Some(challenge_resp.clone()),
                        None,
                    ) {
                        Err(e) => {
                            self.metrics.error.increment(1);
                            log::warn!(target: "request::session", "[{src}] challenge verification failed for session_id={session_id}: {e:?}");
                            return Some((
                                noop_ack(),
                                Packet {
                                    session_id: session_id.to_vec(),
                                    kind: Some(packet::Kind::Response(Response {
                                        code: StatusCode::BadRequest.into(),
                                        request_id,
                                        kind: None,
                                    })),
                                },
                            ));
                        }
                        Ok(v) => v,
                    };

                    if !self.check_session_id(session_id, src) {
                        self.metrics.error.increment(1);
                        return Some((
                            noop_ack(),
                            Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::BadRequest.into(),
                                    request_id,
                                    kind: Some(response::Kind::Session(Default::default())),
                                })),
                            },
                        ));
                    }

                    match self.session_manager.new_session(
                        clock,
                        session_id,
                        src,
                        node_id,
                        keys,
                        supported_encryptions.clone(),
                    ) {
                        Ok(_) => Some((
                            self.challenge_valid_ack.clone(),
                            Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::Ok.into(),
                                    request_id,
                                    kind: Some(response::Kind::Session(Default::default())),
                                })),
                            },
                        )),
                        Err(prev_session_id) => {
                            if prev_session_id.node_id != node_id {
                                log::warn!(target: "request::session", "[{src}] conflicting session_id={session_id}, age={:?} old({}) != {node_id}", clock.age(&prev_session_id.ts), prev_session_id.node_id);
                                return Some((
                                    noop_ack(),
                                    Packet {
                                        session_id: session_id.to_vec(),
                                        kind: Some(packet::Kind::Response(Response {
                                            code: StatusCode::Conflict.into(),
                                            request_id,
                                            kind: Some(response::Kind::Session(Default::default())),
                                        })),
                                    },
                                ));
                            } else {
                                Some((
                                    self.challenge_valid_ack.clone(),
                                    Packet {
                                        session_id: session_id.to_vec(),
                                        kind: Some(packet::Kind::Response(Response {
                                            code: StatusCode::Ok.into(),
                                            request_id,
                                            kind: Some(response::Kind::Session(Default::default())),
                                        })),
                                    },
                                ))
                            }
                        }
                    }
                }
                p => {
                    log::warn!("invalid {:?}", p);
                    None
                }
            }
        } else {
            let (mut session, _challenge) = challenge::prepare_challenge_response(self.difficulty);
            let session_id = self.gen_new_challenge(src, self.epoch());

            if let Some(s) = &mut session.challenge_req {
                s.challenge = self.session_challenge(session_id).to_vec();
                log::info!(
                    "req session_id={}, request_id={}, challange={}",
                    session_id,
                    request_id,
                    hex::encode(s.challenge.as_slice())
                );
            }

            self.metrics.start.increment(1);
            log::debug!(target: "request::session", "[{src}] Starting session {}", session_id);
            Some((
                self.challenge_send_ack.clone(),
                Packet {
                    session_id: session_id.to_vec(),
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
