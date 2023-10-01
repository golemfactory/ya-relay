use crate::client::SessionError;
use backoff::future::retry;
use backoff::Error::Transient;
use backoff::{Error, ExponentialBackoff};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::cmp::min;
use std::future::Future;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tokio::time::Instant;
use ya_relay_core::{server_session, NodeId};

use crate::direct_session::DirectSession;
use crate::session::network_view::{NodeAwaiting, NodeView};
use crate::session::session_state::SessionState;
use crate::session::SessionLayer;

pub async fn track_sessions_expiration(layer: SessionLayer) {
    let expiration = layer.config.session_expiration;

    loop {
        log::trace!("[expire]: Checking, if all sessions are alive. Removing not active sessions.");

        let next_session_expiration = timeout_sessions(layer.clone(), expiration).await;
        let next_slot_expiration = timeout_relay_slots(layer.clone(), expiration).await;

        let sleep = min(next_session_expiration, next_slot_expiration);

        log::trace!("Next sessions / relayed slots cleanup in {:?}", sleep);
        tokio::time::sleep(sleep).await;
    }
}

// Checks for live sessions and calculates the sleep time until the next session expiration
async fn timeout_sessions(layer: SessionLayer, expiration: Duration) -> Duration {
    let now = Instant::now();

    let sessions = layer
        .sessions()
        .await
        .into_iter()
        .filter_map(|session| session.upgrade())
        .collect::<Vec<_>>();

    // Collect futures in vector and execute asynchronously, because pinging
    // can last a few seconds especially in case of inactive sessions.
    let ping_futures = sessions
        .iter()
        .map(|session| session.raw.keep_alive(expiration));

    let last_seen = futures::future::join_all(ping_futures).await;

    // Collect indices of Sessions to close.
    let (expired, live): (Vec<_>, Vec<_>) = last_seen
        .into_iter()
        .enumerate()
        .partition(|(i, timestamp)| (*timestamp + expiration) < now);

    log::trace!("Closing {} expired sessions.", expired.len());

    let close_session_futures = expired.into_iter().map(|(idx, _)| {
        let session = sessions[idx].clone();
        let layer = layer.clone();
        async move {
            log::info!(
                "Closing session {} ({}) not responding to ping.",
                session.raw.id,
                session.raw.remote
            );

            layer.close_session(session).await;
        }
    });
    futures::future::join_all(close_session_futures).await;

    let next_expiration = live.into_iter().map(|(_, ts)| ts).min().unwrap_or(now) + expiration;
    next_expiration.saturating_duration_since(Instant::now())
}

async fn timeout_relay_slots(layer: SessionLayer, expiration: Duration) -> Duration {
    let node_info_futures = match layer.get_server_session().await {
        Some(server_session) => server_session
            .list()
            .into_iter()
            .map(|node| {
                let server_session = server_session.clone();
                async move {
                    let node_id = node.default_id;
                    let node_info = server_session.raw.find_node(node.default_id).await;
                    (node_id, node_info)
                }
            })
            .collect::<Vec<_>>(),
        None => vec![],
    };
    log::debug!("{} relay slots to ping", node_info_futures.len());
    let last_seen_relayed = futures::future::join_all(node_info_futures).await;

    let now = Utc::now();
    let elapsed = now - expiration - chrono::Duration::seconds(42);
    let (expired, live): (Vec<_>, Vec<_>) = last_seen_relayed
        .into_iter()
        .map(|(node_id, node_info)| {
            let last_seen = node_info.map(|node_info| {
                DateTime::<Utc>::from_timestamp(node_info.seen_ts as i64, 0)
                    .map(|dt| dt + expiration)
                    .unwrap_or(elapsed)
            });
            (node_id, last_seen)
        })
        .partition(|(node_id, last_seen_result)| match last_seen_result {
            Ok(last_seen) => *last_seen + expiration < now,
            Err(_) => true,
        });

    let disconnect_futures = expired
        .iter()
        .map(|(node_id, _)| {
            log::trace!("Closing relayed connection to {}.", node_id);
            layer.disconnect(*node_id)
        })
        .collect::<Vec<_>>();

    futures::future::join_all(disconnect_futures).await;

    let next_expiration = live
        .into_iter()
        .map(|(_, ts)| ts.unwrap())
        .min()
        .unwrap_or(now)
        + expiration;
    let sleep = next_expiration - Utc::now();
    if sleep < chrono::Duration::zero() {
        Duration::from_secs(0)
    } else {
        sleep.to_std().unwrap_or(Duration::from_secs(0))
    }
}
