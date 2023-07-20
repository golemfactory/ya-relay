use anyhow::anyhow;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;

use ya_relay_core::dispatch::{dispatch, Dispatcher, Handler};
use ya_relay_core::error::ServerResult;
use ya_relay_core::server_session::{Endpoint, SessionId};
use ya_relay_core::session::Session;
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_proto::proto;

/// Tests if other Node has public IP.
/// Uses separate socket on different port.
#[derive(Clone)]
pub struct EndpointsChecker {
    out_socket: OutStream,
    state: Arc<RwLock<EndpointsCheckerState>>,
}

pub struct EndpointsCheckerState {
    sessions: HashMap<SocketAddr, Arc<Session>>,
}

impl EndpointsChecker {
    pub async fn spawn(config: Arc<Config>) -> anyhow::Result<EndpointsChecker> {
        // Spawn new socket only for discovering IPs on the same address, but different port.
        let mut address = config.address.clone();
        address
            .set_port(Some(config.ip_checker_port))
            .map_err(|_| anyhow!("Unable to set port."))?;
        address
            .set_scheme("Udp")
            .map_err(|_| anyhow!("Unable to set Udp scheme."))?;

        log::info!(
            "Binding socket for checking public endpoints on {}",
            address
        );

        let (input, output, _addr) = udp_bind(&address).await?;

        let checker = EndpointsChecker {
            out_socket: output,
            state: Arc::new(RwLock::new(EndpointsCheckerState {
                sessions: Default::default(),
            })),
        };

        tokio::task::spawn_local(dispatch(checker.clone(), input));
        Ok(checker)
    }

    async fn new_session(&self, session_id: SessionId, addr: &SocketAddr) -> Arc<Session> {
        let session = Session::new(*addr, session_id, self.out_socket.clone());
        {
            self.state
                .write()
                .await
                .sessions
                .insert(*addr, session.clone());
        };
        session
    }

    async fn remove_session(&self, session: Arc<Session>) {
        self.state.write().await.sessions.remove(&session.remote);
    }

    pub async fn public_endpoints(
        &self,
        session_id: SessionId,
        addr: &SocketAddr,
    ) -> ServerResult<Vec<Endpoint>> {
        let session = self.new_session(session_id, addr).await;

        // We need separate socket to check, if Node's address is public.
        // If we would use the same socket as always, we would be able to
        // send packets even to addresses behind NAT.
        let endpoints = match session.ping().await {
            Ok(_) => vec![Endpoint {
                protocol: proto::Protocol::Udp,
                address: *addr,
            }],
            // Ping timed out. Node doesn't have public IP
            Err(_) => vec![],
        };

        self.remove_session(session).await;
        Ok(endpoints)
    }

    fn incorrect_packet(packet_type: &str, from: SocketAddr) {
        log::warn!(
            "{} packet received on Socket for discovering endpoints (from {}). \
            Correct client implementation shouldn't send this. This could be either bug \
            behavior or incorrect configuration.",
            packet_type,
            from,
        );
    }
}

impl Handler for EndpointsChecker {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        let handler = self.clone();
        async move {
            handler
                .state
                .read()
                .await
                .sessions
                .get(&from)
                .map(|session| session.dispatcher())
        }
        .boxed_local()
    }

    #[inline]
    fn on_control(
        self,
        _session_id: Vec<u8>,
        _control: proto::Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        Self::incorrect_packet("Control", from);
        None
    }

    #[inline]
    fn on_request(
        self,
        _session_id: Vec<u8>,
        _request: proto::Request,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        Self::incorrect_packet("Request", from);
        None
    }

    #[inline]
    fn on_forward(
        self,
        _forward: proto::Forward,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        Self::incorrect_packet("Forward", from);
        None
    }
}
