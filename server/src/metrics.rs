use chrono::{DateTime, Utc};

use metrics::{describe_histogram, register_counter, register_histogram, Unit};
use metrics_exporter_prometheus::PrometheusBuilder;

pub fn register_metrics(addr: std::net::SocketAddr) {
    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_quantiles(&[
            0.0, 0.01, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 0.999,
        ])
        .expect("Metrics initialization failure");

    log::info!("Metrics http listener at {addr}");
    builder.install().expect("Metrics installation failure");

    register_counter!("ya-relay.packet.incoming.error");
    register_counter!("ya-relay.packet.incoming");
    register_counter!("ya-relay.packet.dropped");
    register_counter!("ya-relay.session.created");
    register_counter!("ya-relay.session.purged");
    register_counter!("ya-relay.session.removed");

    register_counter!("ya-relay-core.packet.incoming.size");
    register_counter!("ya-relay-core.packet.outgoing.size");

    register_counter!("ya-relay.packet.neighborhood");
    register_counter!("ya-relay.packet.node-info");
    register_counter!("ya-relay.packet.slot-info");
    register_counter!("ya-relay.packet.reverse-connection");
    register_counter!("ya-relay.packet.ping");
    register_counter!("ya-relay.packet.disconnect");
    register_counter!("ya-relay.packet.forward");

    register_counter!("ya-relay.packet.neighborhood.done");
    register_counter!("ya-relay.packet.node-info.done");
    register_counter!("ya-relay.packet.slot-info.done");
    register_counter!("ya-relay.packet.reverse-connection.done");
    register_counter!("ya-relay.packet.ping.done");
    register_counter!("ya-relay.packet.disconnect.done");
    register_counter!("ya-relay.packet.forward.done");

    register_counter!("ya-relay.packet.neighborhood.error");
    register_counter!("ya-relay.packet.node-info.error");
    register_counter!("ya-relay.packet.slot-info.error");
    register_counter!("ya-relay.packet.reverse-connection.error");
    register_counter!("ya-relay.packet.ping.error");
    register_counter!("ya-relay.packet.disconnect.error");
    register_counter!("ya-relay.packet.forward.error");

    register_histogram!("ya-relay.packet.neighborhood.processing-time");

    register_counter!("ya-relay.session.establish.start");
    register_counter!("ya-relay.session.establish.finished");
    register_counter!("ya-relay.session.establish.challenge.valid");
    register_counter!("ya-relay.session.establish.challenge.sent");
    register_counter!("ya-relay.session.establish.register");
    register_counter!("ya-relay.session.establish.error");

    register_histogram!("ya-relay.packet.processing-time");
    register_histogram!("ya-relay.packet.response-time");

    describe_histogram!(
        "ya-relay.packet.neighborhood.processing-time",
        Unit::Microseconds,
        "Processing time for Neighborhood packet"
    );
    describe_histogram!(
        "ya-relay.packet.processing-time",
        Unit::Microseconds,
        "Time between packet processing start and finish."
    );
    describe_histogram!(
        "ya-relay.packet.response-time",
        Unit::Microseconds,
        "Time between receiving packet and finishing processing (responding if applicable)."
    );
}

pub fn elapsed_metric(since: DateTime<Utc>) -> f64 {
    (Utc::now() - since)
        .num_microseconds()
        .map(|t| t as f64)
        .unwrap_or(f64::MAX)
}
