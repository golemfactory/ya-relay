use std::sync::Arc;
use std::time;

use semver::Version;
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

#[derive(Debug)]
enum ClientVersionError {
    Invalid(semver::Error),
    TooOld(Version),
}

fn validate_client_version(
    client_version: &str,
    min_client_version: &Version,
) -> Result<Version, ClientVersionError> {
    let version = Version::parse(client_version).map_err(ClientVersionError::Invalid)?;
    if &version < min_client_version {
        return Err(ClientVersionError::TooOld(version));
    }
    Ok(version)
}

#[derive(clap::Args, Clone)]
/// Ip Checker configuration args
#[command(next_help_heading = "Session handler options")]
pub struct SessionHandlerConfig {
    #[arg(long, env, default_value = "16")]
    pub difficulty: u64,
    #[arg(long, env, value_parser = u128_from_hex)]
    pub salt: Option<u128>,
    #[arg(long, env = "RELAY_MIN_CLIENT_VERSION", default_value = "0.7.0")]
    pub min_client_version: Version,
}

fn u128_from_hex(hex_str: &str) -> Result<u128, hex::FromHexError> {
    let bytes: [u8; 16] = hex::FromHex::from_hex(hex_str)?;
    Ok(u128::from_le_bytes(bytes))
}

pub struct SessionHandler {
    difficulty: u64,
    salt: [u8; 16],
    min_client_version: Version,
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
        let min_client_version = config.min_client_version.clone();

        Self {
            difficulty,
            salt,
            min_client_version,
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

                    let (node_id, keys, session_key_with_proofs) =
                        match challenge::recover_identities_from_challenge_with_proof::<
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
                        session_key_with_proofs,
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
                                Some((
                                    noop_ack(),
                                    Packet {
                                        session_id: session_id.to_vec(),
                                        kind: Some(packet::Kind::Response(Response {
                                            code: StatusCode::Conflict.into(),
                                            request_id,
                                            kind: Some(response::Kind::Session(Default::default())),
                                        })),
                                    },
                                ))
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
            let client_version = match validate_client_version(
                &req_session.client_version,
                &self.min_client_version,
            ) {
                Ok(version) => version,
                Err(ClientVersionError::TooOld(version)) => {
                    self.metrics.error.increment(1);
                    log::warn!(
                        target: "request::session",
                        "[{src}] rejecting client version {version}; minimum supported version is {}",
                        self.min_client_version
                    );
                    return Some((
                        noop_ack(),
                        Packet {
                            session_id: Vec::new(),
                            kind: Some(packet::Kind::Response(Response {
                                code: StatusCode::BadRequest.into(),
                                request_id,
                                kind: Some(response::Kind::Session(Default::default())),
                            })),
                        },
                    ));
                }
                Err(ClientVersionError::Invalid(error)) => {
                    self.metrics.error.increment(1);
                    log::warn!(
                        target: "request::session",
                        "[{src}] rejecting invalid client version {:?}: {error}",
                        req_session.client_version
                    );
                    return Some((
                        noop_ack(),
                        Packet {
                            session_id: Vec::new(),
                            kind: Some(packet::Kind::Response(Response {
                                code: StatusCode::BadRequest.into(),
                                request_id,
                                kind: Some(response::Kind::Session(Default::default())),
                            })),
                        },
                    ));
                }
            };

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
            log::debug!(
                target: "request::session",
                "[{src}] Starting session {session_id} for client version {client_version}"
            );
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

#[cfg(test)]
mod tests {
    use super::{
        packet, request, validate_client_version, ClientVersionError, Clock, SessionHandler,
        SessionHandlerConfig, SessionManager, StatusCode,
    };

    fn hello_response_code(client_version: &str) -> StatusCode {
        let config = SessionHandlerConfig {
            difficulty: 1,
            salt: Some(0),
            min_client_version: "0.7.0".parse().unwrap(),
        };
        let handler = SessionHandler::new(&SessionManager::new(), &config);
        let request = request::Session {
            client_version: client_version.to_owned(),
            ..Default::default()
        };
        let (_, packet) = handler
            .handle(
                &Clock::now(),
                "127.0.0.1:12345".parse().unwrap(),
                1,
                None,
                &request,
            )
            .unwrap();
        let Some(packet::Kind::Response(response)) = packet.kind else {
            panic!("expected hello response")
        };
        StatusCode::try_from(response.code).unwrap()
    }

    #[test]
    fn rejects_missing_or_invalid_client_version() {
        let minimum = "0.7.0".parse().unwrap();

        assert!(matches!(
            validate_client_version("", &minimum),
            Err(ClientVersionError::Invalid(_))
        ));
        assert!(matches!(
            validate_client_version("development", &minimum),
            Err(ClientVersionError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_client_version_below_minimum() {
        let minimum = "0.7.0".parse().unwrap();

        assert!(matches!(
            validate_client_version("0.6.2", &minimum),
            Err(ClientVersionError::TooOld(version)) if version.to_string() == "0.6.2"
        ));
        assert!(matches!(
            validate_client_version("0.7.0-rc.1", &minimum),
            Err(ClientVersionError::TooOld(_))
        ));
    }

    #[test]
    fn accepts_minimum_and_newer_client_versions() {
        let minimum = "0.7.0".parse().unwrap();

        assert_eq!(
            validate_client_version("0.7.0", &minimum)
                .unwrap()
                .to_string(),
            "0.7.0"
        );
        assert_eq!(
            validate_client_version("0.8.0", &minimum)
                .unwrap()
                .to_string(),
            "0.8.0"
        );
    }

    #[test]
    fn hello_packet_rejects_old_client_and_accepts_current_client() {
        assert_eq!(hello_response_code("0.6.2"), StatusCode::BadRequest);
        assert_eq!(hello_response_code("0.7.0"), StatusCode::Ok);
    }
}
