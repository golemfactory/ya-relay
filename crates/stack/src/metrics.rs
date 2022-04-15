use std::ops::{Add, Mul, Sub};
use std::time::{Duration, Instant};

use num_traits::{One, Zero};

pub const DEFAULT_EWMA_ALPHA_SHORT: f32 = 0.9;
pub const DEFAULT_EWMA_ALPHA_MID: f32 = 0.7;
pub const DEFAULT_EWMA_ALPHA_LONG: f32 = 0.2;

#[derive(Clone, Default)]
pub struct ChannelMetrics {
    pub tx: Metrics,
    pub rx: Metrics,
}

#[derive(Clone)]
pub struct Metrics {
    pub short: TimeWindow,
    pub mid: TimeWindow,
    pub long: TimeWindow,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(
            DEFAULT_EWMA_ALPHA_SHORT,
            DEFAULT_EWMA_ALPHA_MID,
            DEFAULT_EWMA_ALPHA_LONG,
        )
        .unwrap()
    }
}

impl Metrics {
    pub fn new(short_alpha: f32, mid_alpha: f32, long_alpha: f32) -> Result<Self, f32> {
        let now = Instant::now();
        let sample_freq = Duration::from_secs(1);

        Ok(Self {
            short: TimeWindow::new(now, sample_freq, Ewma::new(short_alpha)?),
            mid: TimeWindow::new(now, sample_freq, Ewma::new(mid_alpha)?),
            long: TimeWindow::new(now, sample_freq, Ewma::new(long_alpha)?),
        })
    }

    pub fn push(&mut self, value: f32) {
        let now = Instant::now();
        self.short.push(value, now);
        self.mid.push(value, now);
        self.long.push(value, now);
    }
}

/// Series average abstraction
pub trait Average<T>: Clone {
    fn push(&mut self, value: T);
    fn value(&self) -> T;
}

/// Exponentially weighted moving average
#[derive(Clone)]
pub struct Ewma<T = f32>
where
    T: Clone,
{
    value: Option<T>,
    alpha: T,
    one_min_alpha: T,
}

impl<T> Ewma<T>
where
    T: Zero + One + Sub<Output = T> + PartialOrd + Clone,
{
    pub fn new(alpha: T) -> Result<Self, T> {
        let zero = T::zero();
        let one = T::one();

        if alpha < zero || alpha > one {
            return Err(alpha);
        }

        let one_min_alpha = one.sub(alpha.clone());

        Ok(Self {
            value: None,
            alpha,
            one_min_alpha,
        })
    }
}

impl<T> Average<T> for Ewma<T>
where
    T: Zero + Add<Output = T> + Mul<Output = T> + Clone,
{
    fn push(&mut self, value: T) {
        let new_value = match self.value.take() {
            Some(v) => self.alpha.clone().mul(value) + self.one_min_alpha.clone().mul(v),
            None => value,
        };
        self.value.replace(new_value);
    }

    #[inline]
    fn value(&self) -> T {
        self.value.clone().unwrap_or_else(T::zero)
    }
}

#[derive(Clone)]
pub struct TimeWindow<V = f32, A = Ewma<V>>
where
    A: Average<V>,
{
    size: Duration,
    sampled: Instant,
    updated: Instant,
    acc: V,
    total: V,
    average: A,
}

impl<V, A> TimeWindow<V, A>
where
    V: Zero,
    A: Average<V>,
{
    pub fn new(start: Instant, size: Duration, average: A) -> Self {
        Self {
            size,
            sampled: start - size,
            updated: start - size,
            acc: V::zero(),
            total: V::zero(),
            average,
        }
    }
}

impl<V, A> TimeWindow<V, A>
where
    V: Add<Output = V> + PartialEq + Zero + Clone,
    A: Average<V>,
{
    #[inline]
    pub fn average(&mut self, time: Instant) -> V {
        self.advance(time);
        self.average.value()
    }

    #[inline]
    pub fn sum(&self) -> V {
        self.total.clone()
    }

    pub fn push(&mut self, mut value: V, time: Instant) {
        if time - self.updated < self.size {
            self.acc = self.acc.clone() + value;
            self.sampled = time;
        } else {
            self.advance(time);
            value = value + std::mem::replace(&mut self.acc, V::zero());
            self.push_value(value, time);
        }
    }

    fn advance(&mut self, time: Instant) {
        if time <= self.updated {
            return;
        }

        let size_ms = self.size.as_millis() as f64;
        let cycles = ((time - self.updated).as_millis() as f64 + size_ms / 2.) / size_ms;
        let samples = (cycles as usize).saturating_sub(1);

        if samples > 0 && !self.acc.eq(&V::zero()) {
            let value = std::mem::replace(&mut self.acc, V::zero());
            self.push_value(value, self.sampled);
        }

        for _ in 0..samples {
            self.push_value(V::zero(), self.updated + self.size);
        }
    }

    fn push_value(&mut self, value: V, time: Instant) {
        self.total = self.total.clone().add(value.clone());
        self.average.push(value);
        self.sampled = time;
        self.updated = time;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::metrics::{Ewma, TimeWindow};

    // approximate equality for floating-point number repr
    fn assert_approx_eq(val: f64, expected: f64) {
        assert!(val > expected - 0.01);
        assert!(val < expected + 0.01);
    }

    #[test]
    fn time_window_ewma_swift() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(4);

        let avg = Ewma::new(0.8_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        while now <= until {
            tw.push(0.1, now);
            now += Duration::from_millis(sample_freq.as_millis() as u64 / 10);
        }
        assert_approx_eq(tw.average(now), 1.);
    }

    #[test]
    fn time_window_ewma_steady() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(4);

        let avg = Ewma::new(0.8_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        while now <= until {
            tw.push(123., now);
            now += sample_freq;
        }
        assert_approx_eq(tw.average(now), 123.);
    }

    #[test]
    fn time_window_ewma_tardy() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(8);

        let avg = Ewma::new(0.2_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        while now <= until {
            tw.push(1., now);
            now += sample_freq * 2;
        }

        assert_approx_eq(tw.average(now), 0.5);
    }
}
