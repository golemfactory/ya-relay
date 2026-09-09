use crate::session::network_view::NodeAwaiting;
use crate::session::SessionLayer;
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
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
        layer
            .registry
            .get_entry(server_node_id)
            .await
            .map(|entry| entry.awaiting_notifier())
    }
}

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let anchor = ServerSessionAnchor::new(layer.config.server_session_reconnect_max_interval);

    loop {
        anchor.establish_server_session(&layer).await;
        // Get a fresh notifier for every established server session. Reusing a
        // receiver that observed a terminal state can turn this loop into a busy loop.
        let Some(mut awaiting_notifier) = anchor.get_awaiting_notifier(&layer).await else {
            // The established session was retired before we subscribed. Reconnect ourselves.
            continue;
        };

        // Once server session is established, wait until it is closed or failed.
        if let Err(error) = awaiting_notifier.await_for_closed_or_failed().await {
            log::trace!("[keep-alive]: server session ended: {error}");
        }

        log::trace!("[keep-alive]: establishing server session");
    }
}

#[cfg(test)]
mod tests {
    use super::ServerSessionAnchor;
    use crate::config::ClientBuilder;
    use crate::session::SessionLayer;
    use std::sync::Arc;
    use std::time::Duration;
    use url::Url;
    use ya_relay_core::NodeId;

    #[actix_rt::test]
    async fn missing_or_retired_entry_returns_to_reconnect_instead_of_waiting() {
        let config = ClientBuilder::from_url(Url::parse("udp://127.0.0.1:7477").unwrap())
            .build_config()
            .await
            .unwrap();
        let layer = SessionLayer::new(Arc::new(config));
        let anchor = ServerSessionAnchor::new(Duration::from_secs(1));
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            anchor.get_awaiting_notifier(&layer)
        )
        .await
        .unwrap()
        .is_none());

        layer
            .registry
            .guard(NodeId::default(), &[layer.config.srv_addr])
            .await;

        let mut notifier = anchor.get_awaiting_notifier(&layer).await.unwrap();
        layer.registry.remove_node(NodeId::default()).await;
        assert!(notifier.await_for_closed_or_failed().await.is_err());
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            anchor.get_awaiting_notifier(&layer)
        )
        .await
        .unwrap()
        .is_none());
    }
}
