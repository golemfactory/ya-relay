use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::Not;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{io, mem};

use bytes::BytesMut;
use clap::Args;
use tokio::sync::mpsc;
use tokio::task::{spawn_local, JoinHandle};
use tokio::time;
use tokio::time::sleep;

use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto::{packet, request, response, Message, Packet, Response};

use crate::udp_server::{PacketType, UdpSocket, UdpSocketConfig};
use crate::{SessionRef, SessionWeakRef};

const MAX_PING_SIZE: usize = 100;

#[derive(Args, Clone)]
/// Ip Checker configuration args
#[command(next_help_heading = "IP Checker options")]
pub struct IpCheckerConfig {
    #[arg(long = "ip-check-timeout", env = "IP_CHECK_TIMEOUT", value_parser = humantime::parse_duration, default_value = "1s")]
    /// maximum waiting time for a response when testing public ip.
    pub timeout: Duration,
    #[arg(
        long = "ip-check-retry-cnt",
        env = "IP_CHECK_RETRY_CNT",
        default_value_t = 3
    )]
    /// maximum number of ping message retries
    pub retry_cnt: usize,
    #[arg(long = "ip-check-retry", env = "IP_CHECK_RETRY", value_parser = humantime::parse_duration, default_value = "200ms")]
    /// interval between ping retry sending
    pub retry_after: Duration,
}

impl IpCheckerConfig {
    pub fn build(&self, ip_addr: IpAddr) -> io::Result<IpChecker> {
        checker(ip_addr, self.retry_after, self.timeout, self.retry_cnt)
    }
}

pub struct IpChecker {
    tx: mpsc::UnboundedSender<CheckIpRequestRef>,
    retry_job: JoinHandle<()>,
    retry_cnt: usize,
}

impl IpChecker {
    pub fn check_ip_status<F: FnOnce(bool, SessionRef) + 'static>(
        &self,
        ts: Instant,
        session_ref: SessionRef,
        resolve: F,
    ) -> bool {
        let session_w = Arc::downgrade(&session_ref);
        let addr = session_ref.peer;
        let retry_cnt = self.retry_cnt;
        let resolve = Box::new(resolve);
        let request = Box::new(CheckIpRequest {
            session_w,
            addr,
            ts,
            retry_cnt,
            resolve,
        });
        self.tx.send(request).is_ok()
    }
}

impl Drop for IpChecker {
    fn drop(&mut self) {
        self.retry_job.abort();
    }
}

struct CheckIpRequest {
    session_w: SessionWeakRef,
    addr: SocketAddr,
    ts: Instant,
    retry_cnt: usize,
    resolve: Box<dyn FnOnce(bool, SessionRef)>,
}

type CheckIpRequestRef = Box<CheckIpRequest>;

fn checker(
    ip: IpAddr,
    retry_after: Duration,
    timeout: Duration,
    retry_cnt: usize,
) -> io::Result<IpChecker> {
    let (tx, rx) = mpsc::unbounded_channel::<CheckIpRequestRef>();
    let checker_socket = UdpSocketConfig::new().recv_err().bind((ip, 0).into())?;
    let requests = RefCell::new(BTreeMap::new());
    let (worker_idx, queue_size_gauge) = metrics::new_ip_check_requests();

    let state = Rc::new(IpCheckerState {
        checker_socket,
        requests,
    });

    queue_size_gauge.set(0f64);

    let retry_job = {
        let state = Rc::downgrade(&state);

        spawn_local(async move {
            log::warn!("[{worker_idx}] ip-check retry started");
            while let Some(state) = state.upgrade() {
                if let Err(e) = state.send_pings(timeout).await {
                    log::error!("failed to send ip check: {:?}", e);
                }
                queue_size_gauge.set(state.size() as f64);
                drop(state);
                sleep(retry_after).await
            }
            log::warn!("[{worker_idx}] ip-check retry stopped");
        })
    };

    let recv_job: JoinHandle<io::Result<()>> = {
        let state = Rc::clone(&state);

        spawn_local(async move {
            let mut data = BytesMut::with_capacity(MAX_PING_SIZE);
            loop {
                data.reserve(MAX_PING_SIZE);
                let (peer, packet_type) = match state.checker_socket.recv_any(&mut data).await {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!(target: "service::check_ip", "[{worker_idx}] recv any {:?}", e);
                        time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                state.handle_received(data.split(), peer, packet_type);
            }
        })
    };

    let _ping_job: JoinHandle<Option<()>> = {
        let state = Rc::downgrade(&state);
        let mut rx = rx;

        spawn_local(async move {
            while let Some(req) = rx.recv().await {
                let state = state.upgrade()?;
                match state.do_ping(&req).await {
                    Ok(()) => (),
                    Err(e) => log::error!("[{:?}] failed to send ping: {:?}", req.addr, e),
                }
                let mut g = state.requests.borrow_mut();
                let reqs = g.entry(req.addr).or_insert_with(Vec::new);
                // addr new request if session does not exists.
                if reqs.iter().all(|pending_req| {
                    !SessionWeakRef::ptr_eq(&req.session_w, &pending_req.session_w)
                }) {
                    reqs.push(req);
                }
                drop(g);
                drop(state);
            }
            None
        })
    };

    drop(recv_job);

    Ok(IpChecker {
        tx,
        retry_job,
        retry_cnt,
    })
}

struct IpCheckerState {
    checker_socket: UdpSocket,
    requests: RefCell<BTreeMap<SocketAddr, Vec<CheckIpRequestRef>>>,
}

impl IpCheckerState {
    async fn do_ping(&self, req: &CheckIpRequest) -> anyhow::Result<()> {
        if let Some(session) = req.session_w.upgrade() {
            let session_id = session.session_id.to_vec();
            let data = Packet::request(session_id, request::Ping {}).encode_to_vec();
            log::debug!(target: "service::check_ip", "[{}] sending ping for {}", session.peer, session.session_id);
            self.checker_socket.send_to(&data, req.addr).await?;
        }
        Ok(())
    }

    fn size(&self) -> usize {
        self.requests.borrow().values().map(|reqs| reqs.len()).sum()
    }

    async fn send_pings(&self, timeout: Duration) -> io::Result<()> {
        let mut to_ping = Vec::new();
        {
            let mut g = self.requests.borrow_mut();
            for its in g.values_mut() {
                let new_its = its
                    .drain(..)
                    .filter_map(|mut req| {
                        let session_ref = req.session_w.upgrade()?;

                        if req.ts.elapsed() > timeout {
                            (req.resolve)(false, session_ref);
                            return None;
                        }
                        if req.retry_cnt > 0 {
                            req.retry_cnt -= 1;
                            to_ping.push(session_ref);
                        }
                        Some(req)
                    })
                    .collect();
                *its = new_its;
            }
            g.retain(|_, reqs| reqs.is_empty().not());
            drop(g);
        };

        for session in to_ping {
            let session_id = session.session_id.to_vec();
            let peer = session.peer;
            let data = Packet::request(session_id, request::Ping {}).encode_to_vec();
            log::debug!(target: "service::check_ip", "[{peer}] sending ping retry for {}", session.session_id);
            self.checker_socket.send_to(&data, peer).await?;
        }
        Ok(())
    }

    fn resolve_success(&self, peer: SocketAddr, session_id: &[u8]) -> anyhow::Result<()> {
        let session_id: SessionId = session_id.try_into()?;
        let check_session = |req: CheckIpRequestRef| {
            let session_ref = req.session_w.upgrade()?;

            if session_id == session_ref.session_id {
                log::debug!(target: "service::check_ip", "[{peer}] resolving {session_id} for node: {}", session_ref.node_id);
                (req.resolve)(true, session_ref);
                return None;
            }

            Some(req)
        };
        let mut g = self.requests.borrow_mut();
        let mut peer_clean = false;
        if let Some(reqs) = g.get_mut(&peer) {
            for req in mem::take(reqs) {
                if let Some(req) = check_session(req) {
                    reqs.push(req)
                }
            }
            peer_clean = reqs.is_empty();
        }
        if peer_clean {
            let r = g.remove(&peer);
            debug_assert!(r.unwrap().is_empty());
        }
        Ok(())
    }

    fn handle_received(&self, mut data: BytesMut, peer: SocketAddr, packet_type: PacketType) {
        // An error can refer to an earlier probe or session at the same endpoint.
        // Only a matching Pong or the probe timeout resolves the current check.
        if !matches!(packet_type, PacketType::Data) {
            log::debug!(target: "service::check_ip", "[{peer}] transport event: {packet_type:?}");
            return;
        }
        let packet = match Packet::decode(&mut data) {
            Ok(packet) => packet,
            Err(error) => {
                log::warn!(target: "service::check_ip", "[{peer}] invalid packet: {error:?}");
                return;
            }
        };
        match packet {
            Packet {
                session_id,
                kind:
                    Some(packet::Kind::Response(Response {
                        kind: Some(response::Kind::Pong(_)),
                        ..
                    })),
            } => {
                if let Err(error) = self.resolve_success(peer, &session_id) {
                    log::error!(target: "service::check_ip", "[{peer}] invalid ping response: {error:?}");
                }
            }
            other => log::warn!(target: "service::check_ip", "[{peer}] invalid packet: {other:?}"),
        }
    }
}

mod metrics {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use metrics::*;

    static KEY_IP_CHECK_REQUESTS: Key = Key::from_static_name("ya-relay.ip-check.addrs");
    static WORKER_IDX: AtomicUsize = AtomicUsize::new(0);

    pub fn ip_check_requests(worker_idx: usize) -> Gauge {
        let l = ("worker", worker_idx.to_string());
        let key = KEY_IP_CHECK_REQUESTS.with_extra_labels(vec![(&l).into()]);

        recorder().register_gauge(&key)
    }

    pub fn new_ip_check_requests() -> (usize, Gauge) {
        let worker_idx = WORKER_IDX.fetch_add(1, Ordering::SeqCst);
        (worker_idx, ip_check_requests(worker_idx))
    }
}

#[cfg(test)]
mod test {
    use ya_relay_core::server_session::SessionId;

    use super::*;

    #[test]
    fn test_ping_size() {
        let session_id = SessionId::generate();

        let len = Packet::request(session_id.to_vec(), request::Ping {}).encoded_len();
        assert!(len < MAX_PING_SIZE);
    }

    async fn pending_check() -> (
        IpCheckerState,
        SessionRef,
        Rc<std::cell::Cell<Option<bool>>>,
    ) {
        let manager = crate::SessionManager::new();
        let peer = "127.0.0.1:11501".parse().unwrap();
        let session = manager
            .new_session(
                &crate::state::Clock::now(),
                SessionId::generate(),
                peer,
                Default::default(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .ok()
            .unwrap();
        let outcome = Rc::new(std::cell::Cell::new(None));
        let result = outcome.clone();
        let request = Box::new(CheckIpRequest {
            session_w: Arc::downgrade(&session),
            addr: peer,
            ts: Instant::now(),
            retry_cnt: 0,
            resolve: Box::new(move |success, _| result.set(Some(success))),
        });
        let state = IpCheckerState {
            checker_socket: UdpSocketConfig::new()
                .bind("127.0.0.1:0".parse().unwrap())
                .unwrap(),
            requests: RefCell::new(BTreeMap::from([(peer, vec![request])])),
        };
        (state, session, outcome)
    }

    #[tokio::test]
    async fn delayed_transport_errors_leave_current_probe_pending() {
        let (state, session, outcome) = pending_check().await;
        let pong = Packet::response(
            0,
            session.session_id.to_vec(),
            ya_relay_proto::proto::StatusCode::Ok,
            response::Pong {},
        )
        .encode_to_vec();
        for payload in [&[][..], &[0x0a][..], pong.as_slice()] {
            // Even a complete quoted packet must not resolve a current probe.
            for event in [
                PacketType::Other,
                PacketType::PacketTooBig { mtu: 1280 },
                PacketType::Unreachable(crate::udp_server::UnreachableReason::Port),
            ] {
                state.handle_received(BytesMut::from(payload), session.peer, event);
                assert_eq!(outcome.get(), None);
                assert_eq!(state.size(), 1);
            }
        }
        // Malformed data and a Pong from another session must not resolve it either.
        state.handle_received(BytesMut::from(&[0xff][..]), session.peer, PacketType::Data);
        let stale_pong = Packet::response(
            0,
            SessionId::generate().to_vec(),
            ya_relay_proto::proto::StatusCode::Ok,
            response::Pong {},
        )
        .encode_to_vec();
        state.handle_received(
            BytesMut::from(stale_pong.as_slice()),
            session.peer,
            PacketType::Data,
        );
        assert_eq!(outcome.get(), None);
        state.handle_received(
            BytesMut::from(pong.as_slice()),
            session.peer,
            PacketType::Data,
        );
        assert_eq!(outcome.get(), Some(true));
        assert_eq!(state.size(), 0);
    }

    #[tokio::test]
    async fn probe_still_times_out_after_transport_error() {
        let (state, session, outcome) = pending_check().await;
        state.handle_received(BytesMut::new(), session.peer, PacketType::Other);
        state.requests.borrow_mut().get_mut(&session.peer).unwrap()[0].ts =
            Instant::now() - Duration::from_secs(2);
        state.send_pings(Duration::from_secs(1)).await.unwrap();
        assert_eq!(outcome.get(), Some(false));
        assert_eq!(state.size(), 0);
    }
}
