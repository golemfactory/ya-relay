use crate::direct_session::DirectSession;
use crate::session::network_view::{NodeAwaiting, NodeView};
use crate::session::session_state::SessionState;
use crate::session::session_traits::SessionDeregistration;
use crate::session::SessionLayer;
use backoff::backoff::Backoff;
use backoff::Error::Transient;
use backoff::{Error, ExponentialBackoff};
use futures::future::err;
use log::trace;
use std::time::Duration;
use ya_relay_core::NodeId;

#[derive(Clone)]
struct ServerSessionAnchor {
    backoff_strategy: ExponentialBackoff,
}

impl ServerSessionAnchor {
    pub fn new(max_interval: Duration) -> ServerSessionAnchor {
        ServerSessionAnchor {
            backoff_strategy: ExponentialBackoff {
                multiplier: 2.0,
                max_interval,
                max_elapsed_time: None,
                randomization_factor: 0.99,
                ..Default::default()
            },
        }
    }

    async fn establish_server_session(&self, layer: &SessionLayer) {
        let mut backoff_strategy = self.backoff_strategy.clone();
        backoff_strategy.reset();

        let mut establish_server_session_once = || async {
            let server_session = layer.server_session().await;
            Ok(server_session?)
        };

        let mut notify = |error, duration| {
            log::trace!("Backoff: error={:?}, duration={:?}", error, duration);
        };

        backoff::future::retry_notify(backoff_strategy, establish_server_session_once, notify)
            .await;
    }

    async fn get_awaiting_notifier(&self, layer: &SessionLayer) -> Option<NodeAwaiting> {
        let server_node_id = NodeId::default();
        let entry = layer.registry.get_entry(server_node_id).await;
        loop {
            if entry.is_some() {
                return entry.as_ref().map(|entry| entry.awaiting_notifier());
            }
            //Wait for server session entry to be created
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let mut awaiting_notifier: Option<NodeAwaiting> = None;
    let mut anchor = ServerSessionAnchor::new(layer.config.server_session_reconnect_max_interval);

    loop {
        // Get awaiting notifier for server session, this will poll with sleep if needed
        if awaiting_notifier.is_none() {
            awaiting_notifier = anchor.get_awaiting_notifier(&layer).await;
        }

        //Once server session is established, then wait until it is closed or failed.
        awaiting_notifier
            .as_mut()
            .unwrap()
            .await_for_closed_or_failed()
            .await;

        log::trace!("[keep-alive]: establishing server session");
        //Re-establish server session using retry policy with exponential backoff.
        let server_session = anchor.establish_server_session(&layer).await;
    }
}
