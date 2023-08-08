use crate::client::SessionError;
use std::cmp::min;
use std::future::Future;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tokio::time::Instant;
use ya_relay_core::NodeId;
use backoff::{Error, ExponentialBackoff};


use crate::direct_session::DirectSession;
use crate::session::network_view::{NodeAwaiting, NodeView};
use crate::session::session_state::SessionState;
use crate::session::session_traits::SessionDeregistration;
use crate::session::SessionLayer;

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let mut time_to_reconnect = std::time::Duration::from_secs(1);
    let time_to_check = std::time::Duration::from_secs(5);

    tokio::time::sleep(time_to_check).await;

    let server_node_id = NodeId::default();

    let get_awaiting_notifier = || async {
        let entry = layer.registry.get_entry(server_node_id).await;
        match entry {
            None => None,
            Some(entry) => Some(entry.awaiting_notifier()),
        }
    };

    let mut awaiting_notifier = get_awaiting_notifier().await;

    let get_server_session = || async {
        layer.server_session().await
    };


    loop {
        let server_session = get_server_session().await;

        // let mut op = || async {
        //     let server_session = get_server_session().await;
        //     match server_session {
        //         Ok(_) => {
        //             if awaiting_notifier.is_some() {
        //                 awaiting_notifier.as_mut()
        //                     .unwrap()
        //                     .await_for_closed_or_failed()
        //                     .await;
        //             }
        //             // Connection was successful so reset time to reconnect after failure to default value
        //             time_to_reconnect = std::time::Duration::from_secs(1);
        //
        //             log::debug!("Server session keep alive");
        //             Ok(time_to_reconnect)
        //         }
        //         Err(_) => {
        //             time_to_reconnect = min(
        //                 time_to_reconnect * 2,
        //                 layer.config.server_session_reconnect_max_interval,
        //             );
        //
        //             log::error!(
        //                 "Server session error. Reconnect in {:?}",
        //                 time_to_reconnect
        //             );
        //             Err(time_to_reconnect)
        //         }
        //     }
        // };


        let mut op = || async {
            let server_session = get_server_session().await;
            server_session
        };

        sleep(Duration::from_secs(1));

        let mut backoff = ExponentialBackoff::default();
        let retry_result = retry(backoff, op);
        println!("Retry result: {:?}", retry_result);
    };


    //
    // loop {
    //
    //     let server_session = get_server_session().await;
    //
    //     let sleep_time = match server_session {
    //         Ok(_) => {
    //             if awaiting_notifier.is_some() {
    //                 awaiting_notifier.as_mut()
    //                     .unwrap()
    //                     .await_for_closed_or_failed()
    //                     .await;
    //             } else {
    //                 awaiting_notifier = get_awaiting_notifier().await;
    //             }
    //             // Connection was successful so reset time to reconnect after failure to default value
    //             time_to_reconnect = std::time::Duration::from_secs(1);
    //
    //             log::debug!("Server session keep alive");
    //             Ok(time_to_reconnect)
    //         }
    //         Err(_) => {
    //             time_to_reconnect = min(
    //                 time_to_reconnect * 2,
    //                 layer.config.server_session_reconnect_max_interval,
    //             );
    //
    //             log::error!(
    //                 "Server session error. Reconnecting in {}s ...",
    //                 time_to_reconnect.as_secs()
    //             );
    //
    //             Err(time_to_reconnect)
    //         }
    //     };
    //     let t = match sleep_time {
    //         Ok(t) => t,
    //         Err(t) => t
    //     };
    //     tokio::time::sleep(t).await;
    // }
}

pub async fn track_sessions_expiration(layer: SessionLayer) {
    let expiration = Duration::from_secs(5);//layer.config.session_expiration;

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

        log::debug!("Closing {} expired sessions.", expired_idx.len());
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
