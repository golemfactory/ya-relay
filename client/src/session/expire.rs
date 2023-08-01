use std::cmp::min;
use std::future::Future;
use std::sync::Arc;
use tokio::time::Instant;
use ya_relay_core::NodeId;
use crate::client::SessionError;

use crate::direct_session::DirectSession;
use crate::session::network_view::NodeView;
use crate::session::session_state::SessionState;
use crate::session::session_traits::SessionDeregistration;
use crate::session::SessionLayer;

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let mut time_to_reconnect = std::time::Duration::from_secs(1);
    let time_to_check = std::time::Duration::from_secs(5);

    tokio::time::sleep(std::time::Duration::from_secs(time_to_check.as_secs())).await;

    loop {
        match layer.server_session().await {
            Ok(_) => {
                // Connection was successful so reset time to reconnect after failure to default value
                time_to_reconnect = std::time::Duration::from_secs(1);
                tokio::time::sleep(std::time::Duration::from_secs(time_to_check.as_secs())).await;

                async {
                    let server_node_id = NodeId::default();

                    let entry = layer
                        .registry
                        .get_entry(server_node_id)
                        .await;

                    if let Some(entry) = entry {
                        entry.awaiting_notifier().await_for_closed_or_failed().await;
                    }
                }.await;

                log::debug!("Server session keep alive");
            }
            Err(_) => {
                log::error!("Server session error. Reconnecting in {}s ...", time_to_reconnect.as_secs());
                tokio::time::sleep(std::time::Duration::from_secs(time_to_reconnect.as_secs())).await;
                // If we're not able to reconnect then next time wait longer
                time_to_reconnect = min(time_to_reconnect *2, layer.config.server_session_reconnect_max_interval);
            }
        }
    }
}

pub async fn track_sessions_expiration(layer: SessionLayer) {
    let expiration = layer.config.session_expiration;

    loop {
        log::trace!("Checking, if all sessions are alive. Removing not active sessions.");

        let sessions = layer
            .sessions()
            .await
            .into_iter()
            .filter_map(|session| session.upgrade())
            .collect::<Vec<_>>();
        let now = Instant::now();

        // Collect futures in vector and execute asynchronously, because pinging
        // can last a few seconds especially in case of inactive sessions.
        let ping_futures = sessions
            .iter()
            .map(|session| session.raw.keep_alive(expiration))
            .collect::<Vec<_>>();

        let last_seen = futures::future::join_all(ping_futures).await;

        // Collect indices of Sessions to close.
        let expired_idx = last_seen
            .iter()
            .enumerate()
            .filter_map(|(i, timestamp)| {
                match *timestamp + expiration < now {
                    true => Some(i),
                    false => None,
                }
            })
            .collect::<Vec<_>>();

        close_sessions(layer.clone(), sessions, expired_idx).await;

        let first_to_expiring = last_seen.iter().min().cloned().unwrap_or(now) + expiration;

        log::trace!(
            "Next sessions cleanup: {:?}",
            first_to_expiring.saturating_duration_since(Instant::now())
        );
        tokio::time::sleep_until(first_to_expiring).await;
    }
}

async fn close_sessions(
    layer: SessionLayer,
    sessions: Vec<Arc<DirectSession>>,
    expired: Vec<usize>,
) {
    for idx in expired.into_iter() {
        let session = sessions[idx].clone();

        log::info!(
            "Closing session {} ({}) not responding to ping.",
            session.raw.id,
            session.raw.remote
        );

        layer.close_session(session.clone()).await;
    }
}
