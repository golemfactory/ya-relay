use crate::client::SessionError;
use backoff::future::retry;
use backoff::Error::Transient;
use backoff::{Error, ExponentialBackoff};
use chrono::{DateTime, Utc, NaiveDateTime};
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
            .map(|session| session.raw.keep_alive(expiration));

        let last_seen = futures::future::join_all(ping_futures).await;

        // Collect indices of Sessions to close.
        let (expired, live): (Vec<_>, Vec<_>) = last_seen
            .into_iter()
            .enumerate()
            .partition(|(i, timestamp)| (*timestamp + expiration) < now);

        log::trace!("Closing {} expired sessions.", expired.len());
        close_sessions(layer.clone(), sessions, expired).await;

        let node_info_futures = match layer.get_server_session().await {
            Some(server_session) => {
                let server_sessions = std::iter::repeat(server_session.clone());
                server_session.list().into_iter().zip(server_sessions)
                    .map(|(node, server_session)| async move { (node.default_id.clone(), server_session.raw.find_node(node.default_id).await) })
                .collect::<Vec<_>>()
            },
            None => vec![],
        };
        log::debug!("{} forwards to ping", node_info_futures.len());
        let last_seen_relayed = futures::future::join_all(node_info_futures).await;

        let utc_now = Utc::now();
        let elapsed = utc_now - expiration - chrono::Duration::seconds(42);
        let (expired_relayed, live_relayed): (Vec<_>, Vec<_>) = last_seen_relayed
            .into_iter()
            .map(|(node_id, node_info)| {
                let last_seen = node_info.map(|node_info|
                        DateTime::<Utc>::from_timestamp(node_info.seen_ts as i64, 0)
                            .map(|dt| dt + expiration)
                            .unwrap_or(elapsed)
                        );
                (node_id, last_seen)
            })
            .partition(|(node_id, last_seen_result)| {
                match last_seen_result {
                    Ok(last_seen) => *last_seen + expiration < utc_now,
                    Err(_) => true,
                }
            });

        futures::future::join_all(
            expired_relayed.iter().map(|(node_id, _)| {
                log::info!("Closing relayed connection to {}.", node_id);
                layer.disconnect(node_id.clone())
            })
        ).await;

        let next_expiration_direct = live.into_iter().map(|(_, ts)| ts).min().unwrap_or(now) + expiration;
        let sleep_direct = next_expiration_direct.saturating_duration_since(Instant::now());
        let next_expiration_relayed = live_relayed.into_iter().map(|(_, ts)| ts.unwrap()).min().unwrap_or(utc_now) + expiration;
        let sleep_relayed = {
            let sleep = next_expiration_relayed - Utc::now();
            if sleep < chrono::Duration::zero() {
                Duration::from_secs(0)
            } else {
                sleep.to_std().unwrap_or(Duration::from_secs(0))
            }
        };
        let sleep = min(sleep_direct, sleep_relayed);

        log::trace!("Next sessions cleanup in {:?}", sleep);
        tokio::time::sleep(sleep).await;
    }
}

async fn close_sessions(
    layer: SessionLayer,
    sessions: Vec<Arc<DirectSession>>,
    expired: Vec<(usize, Instant)>,
) {
    for (idx, _) in expired.into_iter() {
        let session = sessions[idx].clone();

        log::info!(
            "Closing session {} ({}) not responding to ping.",
            session.raw.id,
            session.raw.remote
        );

        layer.close_session(session).await;
    }
}

fn prepare_relayed_connection_node_info_futures<'a>(
    server_session: &'a Arc<DirectSession>,
) -> Vec<impl Future<Output = Result<ya_relay_proto::proto::response::Node, anyhow::Error>> + 'a> {
    server_session.list().iter()
        .map(|node| server_session.raw.find_node(node.default_id))
        .collect::<Vec<_>>()
}

async fn disconnect_nodes<T>(
    layer: SessionLayer,
    forwards: Vec<(NodeId, T)>,
) {
    for (node_id, _) in forwards.into_iter() {
        log::info!("Closing relayed connection to {}.", node_id);

        layer.disconnect(node_id).await;
    }
    match layer.server_session().await {
        Ok(session) =>
            log::debug!("{} forwards left", session.list().len()),
        Err(_) => {},
    }
}
