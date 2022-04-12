use std::ops::{Add, Mul, Sub};
use std::time::{Duration, Instant};

use num_traits::{One, Zero};

pub const DEFAULT_EWMA_ALPHA_SHORT: f32 = 0.9;
pub const DEFAULT_EWMA_ALPHA_MID: f32 = 0.7;
pub const DEFAULT_EWMA_ALPHA_LONG: f32 = 0.45;

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

        Ok(Self {
            short: TimeWindow::new(now, Duration::from_millis(500), Ewma::new(short_alpha)?),
            mid: TimeWindow::new(now, Duration::from_secs(1), Ewma::new(mid_alpha)?),
            long: TimeWindow::new(now, Duration::from_secs(1), Ewma::new(long_alpha)?),
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
    sample_freq: Duration,
    updated: Instant,
    average: A,
    sum: V,
}

impl<V, A> TimeWindow<V, A>
where
    V: Zero,
    A: Average<V>,
{
    pub fn new(start: Instant, sample_freq: Duration, average: A) -> Self {
        Self {
            sample_freq,
            updated: start,
            average,
            sum: V::zero(),
        }
    }
}

impl<V, A> TimeWindow<V, A>
where
    V: Add<Output = V> + Zero + Clone + std::fmt::Display,
    A: Average<V>,
{
    pub fn push(&mut self, value: V, time: Instant) {
        self.advance(time);
        self.push_value(value, time);
    }

    #[inline]
    pub fn average(&mut self, time: Instant) -> V {
        self.advance(time);
        self.average.value()
    }

    #[inline]
    pub fn sum(&self) -> V {
        self.sum.clone()
    }

    fn advance(&mut self, time: Instant) {
        if time <= self.updated {
            return;
        }

        let freq = self.sample_freq.as_millis() as f64;
        let dt = ((time - self.updated).as_millis() as f64 + freq / 2.) / freq;
        let samples = (dt as usize).saturating_sub(1);

        for _ in 0..samples {
            self.push_value(V::zero(), self.updated + self.sample_freq);
        }
    }

    fn push_value(&mut self, value: V, time: Instant) {
        self.sum = self.sum.clone().add(value.clone());
        self.average.push(value);
        self.updated = time;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::metrics::{Ewma, TimeWindow};

    #[test]
    fn time_window_ewma_swift() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(4);

        let avg = Ewma::new(0.8_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        while now <= until {
            tw.push(0.1, now);
            assert_eq!(tw.average(now), 0.1);
            now += Duration::from_millis(sample_freq.as_millis() as u64 / 10);
        }
    }

    #[test]
    fn time_window_ewma_steady() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(4);

        let avg = Ewma::new(0.8_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        while now <= until {
            tw.push(0.1, now);
            assert_eq!(tw.average(now), 0.1);
            now += sample_freq;
        }
    }

    #[test]
    fn time_window_ewma_tardy() {
        let mut now = Instant::now();
        let sample_freq = Duration::from_secs(1);
        let until = now + Duration::from_secs(8);

        let avg = Ewma::new(0.8_f64).expect("failed to create an instance of EWMA");
        let mut tw = TimeWindow::new(now, sample_freq, avg);

        let mut i = 0;
        while now <= until {
            tw.push(0.1, now);

            if i == 0 {
                assert_eq!(tw.average(now), 0.1);
            } else {
                assert!(tw.average(now) < 0.1);
                assert!(tw.average(now) > 0.);
            }

            now += sample_freq * 2;
            i += 1;
        }
    }
}
