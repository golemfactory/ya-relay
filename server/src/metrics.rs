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

    register_counter!("ya-relay.packets.incoming.error");
    register_counter!("ya-relay.packets.incoming");
    register_counter!("ya-relay.sessions.created");
    register_counter!("ya-relay.sessions.purged");
    register_counter!("ya-relay.sessions.removed");

    register_counter!("ya-relay.session.establish.start");
    register_counter!("ya-relay.session.establish.finished");
    register_counter!("ya-relay.session.establish.challenge.valid");
    register_counter!("ya-relay.session.establish.challenge.sent");
    register_counter!("ya-relay.session.establish.register");
    register_counter!("ya-relay.session.establish.error");

    register_histogram!("ya-relay.packet.neighborhood.processing-time");
    register_histogram!("ya-relay.packets.processing-time");
    register_histogram!("ya-relay.packets.response-time");

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
