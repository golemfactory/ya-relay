use metrics::Gauge;

pub struct InstanceCountGuard {
    inner : Gauge
}

impl InstanceCountGuard {

    pub fn new(gauge : Gauge) -> Self {
        gauge.increment(1f64);
        Self {
            inner: gauge
        }
    }

}

impl Drop for InstanceCountGuard {
    fn drop(&mut self) {
        self.inner.decrement(1f64);
        log::info!("stop");
    }
}