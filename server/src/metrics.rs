use metrics_exporter_log::LogExporter;
use metrics_runtime::{observers::YamlBuilder, Receiver};
use std::time::Duration;

pub fn register_metrics(interval: Duration) {
    let receiver = Receiver::builder()
        .build()
        .expect("Metrics initialization failure");
    let mut exporter = LogExporter::new(
        receiver.controller(),
        YamlBuilder::new().set_quantiles(&[
            0.0, 0.01, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 0.999,
        ]),
        log::Level::Info,
        interval,
    );
    receiver.install();

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(interval);
        loop {
            interval.tick().await;
            exporter.turn();
        }
    });
}
