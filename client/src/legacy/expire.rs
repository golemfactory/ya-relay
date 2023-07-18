use std::sync::Arc;
use tokio::time::Instant;

use crate::legacy::session_manager::SessionManager;

use ya_relay_core::session::Session;

pub async fn track_sessions_expiration(layer: SessionManager) {
    let expiration = layer.config.session_expiration;

    loop {
        log::trace!("Checking, if all sessions are alive. Removing not active sessions.");

        let sessions = layer.sessions().await;
        let now = Instant::now();

        // Collect futures in vector and execute asynchronously, because pinging
        // can last a few seconds especially in case of inactive sessions.
        let ping_futures = sessions
            .iter()
            .map(|session| session.keep_alive(expiration))
            .collect::<Vec<_>>();

        let last_seen = futures::future::join_all(ping_futures).await;

        // Collect indices of Sessions to close.
        let expired_idx = last_seen
            .iter()
            .enumerate()
            .filter_map(|(i, timestamp)| match *timestamp + expiration < now {
                true => Some(i),
                false => None,
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

async fn close_sessions(layer: SessionManager, sessions: Vec<Arc<Session>>, expired: Vec<usize>) {
    for idx in expired.into_iter() {
        let session = sessions[idx].clone();

        log::info!(
            "Closing session {} ({}) not responding to ping.",
            session.id,
            session.remote
        );

        if session.remote == layer.config.srv_addr {
            let _ = layer.drop_server_session().await;
            continue;
        }

        layer
            .close_session(session.clone())
            .await
            .map_err(|e| {
                log::warn!(
                    "Error closing session {} ({}): {}",
                    session.id,
                    session.remote,
                    e
                )
            })
            .ok();
    }
}
