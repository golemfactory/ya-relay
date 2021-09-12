use rand::distributions::{Distribution, Uniform};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use crate::{Error, Protocol, Result};

#[derive(Default)]
pub struct Allocator {
    taken: BTreeMap<Protocol, BTreeSet<u16>>,
}

impl Allocator {
    const RANGE: RangeInclusive<u16> = 1000..=65535;

    pub fn next(&mut self, protocol: Protocol) -> Result<u16> {
        let mut rng = rand::thread_rng();
        let mut port = Uniform::from(Self::RANGE).sample(&mut rng);
        let taken = self.taken.entry(protocol).or_insert_with(Default::default);

        let range_start = *Self::RANGE.start();
        let mut num = Self::RANGE.len() as i32;

        while num > 0 {
            if !taken.contains(&port) {
                taken.insert(port);
                return Ok(port);
            }
            port = range_start.max(port.overflowing_add(1).0);
            num -= 1;
        }

        Err(Error::Other("no ports available".into()))
    }

    #[allow(unused)]
    pub fn reserve(&mut self, protocol: Protocol, port: u16) -> Result<()> {
        let entry = self.taken.entry(protocol).or_insert_with(Default::default);
        if entry.contains(&port) {
            return Err(Error::Other(format!("port {} is unavailable", port)));
        }
        entry.insert(port);
        Ok(())
    }

    pub fn free(&mut self, protocol: Protocol, port: u16) {
        self.taken
            .entry(protocol)
            .or_insert_with(Default::default)
            .remove(&port);
    }
}
