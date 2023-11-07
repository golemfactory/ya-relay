use lazy_static::lazy_static;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time;
use std::time::{Duration, Instant, SystemTime};

lazy_static! {
    static ref SERVER_START: Instant = Instant::now();
}

pub struct LastSeen {
    ts: AtomicU32,
}

pub struct Clock {
    now: Instant,
    ts: u32,
}

impl Clock {
    pub fn now() -> Self {
        let server_start = SERVER_START.deref();
        let now = Instant::now();

        debug_assert!(*server_start < now);
        let ts = (now - *server_start).as_secs() as u32;

        Self { now, ts }
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
        let ts = last_seen.ts.load(Ordering::Relaxed);
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

pub struct TsDecoder {
    rel_ts: u64,
}

impl TsDecoder {
    pub fn new() -> TsDecoder {
        let sts = SystemTime::now();
        let diff = SERVER_START.elapsed();
        let unix_diff = sts.duration_since(time::UNIX_EPOCH).unwrap();
        let rel_ts = (unix_diff - diff).as_secs();
        TsDecoder { rel_ts }
    }

    #[inline(always)]
    pub fn decode(&self, ts: &LastSeen) -> u64 {
        ts.ts.load(Ordering::Relaxed) as u64 + self.rel_ts
    }
}
