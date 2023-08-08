use crate::direct_session::DirectSession;
use crate::session::network_view::{NodeAwaiting, NodeView};
use crate::session::session_state::SessionState;
use crate::session::session_traits::SessionDeregistration;
use crate::session::SessionLayer;
use backoff::Error::Transient;
use backoff::ExponentialBackoff;
use ya_relay_core::NodeId;

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let mut time_to_reconnect = std::time::Duration::from_secs(1);
    let time_to_check = std::time::Duration::from_secs(5);

    tokio::time::sleep(time_to_check).await;

    let server_node_id = NodeId::default();

    let get_awaiting_notifier = || async {
        let entry = layer.registry.get_entry(server_node_id).await;
        entry.map(|entry| entry.awaiting_notifier())
    };

    let mut awaiting_notifier = get_awaiting_notifier().await;

    let mut establish_server_session_once = || async {
        let server_session = layer.server_session().await;

        log::debug!(
            "keep_alive_server_session: get_server_session: session.is_ok={:?}",
            server_session.is_ok()
        );
        match server_session {
            Ok(session) => Ok(session),
            Err(error) => {
                log::debug!(
                    "keep_alive_server_session: get_server_session: error={}",
                    error
                );
                Err(Transient {
                    err: error,
                    retry_after: None,
                })
            }
        }
    };

    let establish_server_session = || async {
        let mut backoff = ExponentialBackoff {
            multiplier: 5.0,
            max_interval: core::time::Duration::from_secs(300),
            max_elapsed_time: None,
            randomization_factor: 0.99,
            ..Default::default()
        };

        backoff::future::retry(backoff, establish_server_session_once).await
    };

    loop {
        let server_session = establish_server_session().await;

        if awaiting_notifier.is_none() {
            awaiting_notifier = get_awaiting_notifier().await;
        }
        awaiting_notifier
            .as_mut()
            .unwrap()
            .await_for_closed_or_failed()
            .await;
    }
}
