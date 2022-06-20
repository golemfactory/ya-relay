use metrics::{register_counter, register_histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

pub fn register_metrics(addr: std::net::SocketAddr) {
    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_quantiles(&[
            0.0, 0.01, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 0.999,
        ])
        .expect("Metrics initialization failure");
    // builder.add_allowed_address()
    log::info!("Metrics http listener at {}", addr);
    builder.install().expect("Metrics installation failure");

    register_counter!("ya-relay.packets.incoming");
    register_counter!("ya-relay.sessions.created");
    register_counter!("ya-relay.sessions.purged");
    register_counter!("ya-relay.sessions.removed");
    register_histogram!("ya-relay.packet.neighborhood.processing-time");
    register_histogram!("ya-relay.packets.processing-time");
    register_histogram!("ya-relay.packets.response-time");
}
