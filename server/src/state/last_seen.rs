use lazy_static::lazy_static;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

lazy_static! {
    static ref SERVER_START: Instant = Instant::now();
}

pub struct LastSeen {
    ts: AtomicU32,
}

pub struct Clock {
    server_start: &'static Instant,
    now: Instant,
    ts: u32,
}

impl Clock {
    pub fn now() -> Self {
        let server_start = SERVER_START.deref();
        let now = Instant::now();

        debug_assert!(*server_start < now);
        let ts = (now - *server_start).as_secs() as u32;

        Self {
            server_start,
            now,
            ts,
        }
    }

    pub fn time(&self) -> Instant {
        self.now
    }

    pub fn last_seen(&self) -> LastSeen {
        let ts = self.ts.into();
        LastSeen { ts }
    }

    pub fn touch(&self, last_seen: &LastSeen) {
        last_seen.ts.store(self.ts, Ordering::Release);
    }

    pub fn age(&self, last_seen: &LastSeen) -> Duration {
        let ts = last_seen.ts.load(Ordering::Acquire);
        let diff = self.ts.checked_sub(ts).unwrap_or_default();
        Duration::from_secs(diff.into())
    }
}

impl LastSeen {
    pub fn now() -> LastSeen {
        let ts = SERVER_START.elapsed().as_secs() as u32;
        LastSeen { ts: ts.into() }
    }

    pub fn touch(&self) {
        let ts = SERVER_START.elapsed().as_secs() as u32;
        self.ts.store(ts, Ordering::Release);
    }

    pub fn age(&self) -> Duration {
        let ts = self.ts.load(Ordering::Acquire);
        let its = *SERVER_START + Duration::from_secs(ts.into());
        its.elapsed()
    }
}
