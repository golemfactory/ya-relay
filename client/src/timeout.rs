use futures::future::{Either, Future, Map};
use futures::FutureExt;
use std::time::Duration;
use tokio::time::{timeout, Elapsed, Timeout};

pub trait IntoDuration {
    fn into_duration(self) -> Duration;
}

impl IntoDuration for Duration {
    #[inline(always)]
    fn into_duration(self) -> Duration {
        self
    }
}

impl IntoDuration for f32 {
    #[inline(always)]
    fn into_duration(self) -> Duration {
        Duration::from_secs_f32(self)
    }
}

impl IntoDuration for f64 {
    #[inline(always)]
    fn into_duration(self) -> Duration {
        Duration::from_secs_f64(self)
    }
}

impl IntoDuration for u64 {
    #[inline(always)]
    fn into_duration(self) -> Duration {
        Duration::from_secs(self)
    }
}

macro_rules! impl_into_duration {
    ($ty:ty) => {
        impl IntoDuration for $ty {
            fn into_duration(self) -> Duration {
                if self > 0 as $ty {
                    (self as u64).into_duration()
                } else {
                    Duration::default()
                }
            }
        }
    };
}

impl_into_duration!(i8);
impl_into_duration!(u8);
impl_into_duration!(i16);
impl_into_duration!(u16);
impl_into_duration!(i32);
impl_into_duration!(u32);
impl_into_duration!(i64);

type MapType<F> = Map<F, fn(<F as Future>::Output) -> Result<<F as Future>::Output, Elapsed>>;

pub trait IntoTimeoutFuture<D>: Future
where
    Self: Future + Sized,
    D: IntoDuration,
{
    fn timeout(self, duration: Option<D>) -> Either<Timeout<Self>, MapType<Self>>;
}

impl<F, D> IntoTimeoutFuture<D> for F
where
    Self: Future + Sized,
    D: IntoDuration,
{
    fn timeout(self, duration: Option<D>) -> Either<Timeout<Self>, MapType<Self>> {
        match duration {
            Some(d) => Either::Left(timeout(d.into_duration(), self)),
            None => Either::Right(self.map(|v| Result::Ok(v))),
        }
    }
}
