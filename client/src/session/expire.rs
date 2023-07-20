use std::sync::Arc;
use tokio::time::Instant;

use crate::direct_session::DirectSession;
use crate::session::session_traits::SessionDeregistration;
use crate::session::SessionLayer;

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

        layer.unregister_session(session.clone()).await;
    }
}
