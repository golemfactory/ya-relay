use chrono::{DateTime, Utc};

use metrics::{describe_gauge, describe_histogram, register_counter, register_histogram, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

mod instance_count;

pub use instance_count::*;

pub fn register_metrics() -> PrometheusHandle {
    let builder = PrometheusBuilder::new()
        .set_quantiles(&[
            0.0, 0.01, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 0.999,
        ])
        .expect("Metrics initialization failure");

    let handle = builder.install_recorder().unwrap();

    register_counter!("ya-relay.packet.incoming.error");
    register_counter!("ya-relay.packet.incoming");
    register_counter!("ya-relay.packet.dropped");
    register_counter!("ya-relay.packet.timeout");
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

    register_counter!("ya-relay.packet.forward.incoming.size");
    register_counter!("ya-relay.packet.forward.outgoing.size");

    register_histogram!("ya-relay.packet.neighborhood.processing-time");

    register_counter!("ya-relay.session.establish.start");
    register_counter!("ya-relay.session.establish.finished");
    register_counter!("ya-relay.session.establish.challenge.valid");
    register_counter!("ya-relay.session.establish.challenge.sent");
    register_counter!("ya-relay.session.establish.register");
    register_counter!("ya-relay.session.establish.error");

    register_histogram!("ya-relay.packet.processing-time");
    register_histogram!("ya-relay.packet.response-time");

    register_histogram!("ya-relay.session.cleaner.processing-time");
    register_histogram!("ya-relay.forwarding-limiter.processing-time");

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

    describe_gauge!(
        "ya-relay.ipcheck.addrs",
        Unit::Count,
        "Number of Socket Addresses in Queue"
    );

    crate::udp_server::register_metrics();

    handle
}

pub fn elapsed_metric(since: DateTime<Utc>) -> f64 {
    (Utc::now() - since)
        .num_microseconds()
        .map(|t| t as f64)
        .unwrap_or(f64::MAX)
}
