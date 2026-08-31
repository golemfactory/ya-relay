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

    async fn get_awaiting_notifier(&self, layer: &SessionLayer) -> NodeAwaiting {
        let server_node_id = NodeId::default();
        loop {
            if let Some(entry) = layer.registry.get_entry(server_node_id).await {
                return entry.awaiting_notifier();
            }
            // Wait for server session entry to be created.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub async fn keep_alive_server_session(layer: SessionLayer) {
    let anchor = ServerSessionAnchor::new(layer.config.server_session_reconnect_max_interval);

    loop {
        // Get a fresh notifier for every established server session. Reusing a
        // receiver that observed a terminal state can turn this loop into a busy loop.
        let mut awaiting_notifier = anchor.get_awaiting_notifier(&layer).await;

        // Once server session is established, wait until it is closed or failed.
        if let Err(error) = awaiting_notifier.await_for_closed_or_failed().await {
            log::trace!("[keep-alive]: server session ended: {error}");
        }

        log::trace!("[keep-alive]: establishing server session");
        // Re-establish server session using retry policy with exponential backoff.
        anchor.establish_server_session(&layer).await;
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
    async fn awaiting_notifier_observes_entry_created_after_wait_started() {
        let config = ClientBuilder::from_url(Url::parse("udp://127.0.0.1:7477").unwrap())
            .build_config()
            .await
            .unwrap();
        let layer = SessionLayer::new(Arc::new(config));
        let anchor = ServerSessionAnchor::new(Duration::from_secs(1));
        let notifier = anchor.get_awaiting_notifier(&layer);
        tokio::pin!(notifier);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut notifier)
                .await
                .is_err()
        );

        layer
            .registry
            .guard(NodeId::default(), &[layer.config.srv_addr])
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut notifier)
                .await
                .is_ok()
        );
    }
}
