use smoltcp::phy;
use smoltcp::time::Instant;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::metrics::ChannelMetrics;

use ya_relay_util::Payload;

type Pcap = RefCell<Box<dyn phy::PcapSink>>;

/// Network device capable of injecting and extracting packets
pub struct CaptureDevice {
    tx_queue: VecDeque<Vec<u8>>,
    rx_queue: VecDeque<Payload>,
    medium: phy::Medium,
    max_transmission_unit: usize,
    pcap: Option<Pcap>,
    metrics: Rc<RefCell<ChannelMetrics>>,
}

impl Default for CaptureDevice {
    fn default() -> Self {
        Self {
            tx_queue: Default::default(),
            rx_queue: Default::default(),
            medium: Default::default(),
            max_transmission_unit: 1280,
            pcap: Default::default(),
            metrics: Default::default(),
        }
    }
}

impl CaptureDevice {
    pub fn tap(mtu: usize) -> Self {
        Self {
            max_transmission_unit: mtu,
            ..Default::default()
        }
    }

    pub fn tun(mtu: usize) -> Self {
        Self {
            medium: phy::Medium::Ip,
            max_transmission_unit: mtu,
            ..Default::default()
        }
    }

    pub fn pcap_tap<S>(mtu: usize, mut pcap: S) -> Self
    where
        S: phy::PcapSink + 'static,
    {
        pcap.global_header(phy::PcapLinkType::Ethernet);
        let pcap = RefCell::new(Box::new(pcap));

        Self {
            max_transmission_unit: mtu,
            pcap: Some(pcap),
            ..Default::default()
        }
    }

    pub fn pcap_tun<S>(mtu: usize, mut pcap: S) -> Self
    where
        S: phy::PcapSink + 'static,
    {
        pcap.global_header(phy::PcapLinkType::Ip);
        let pcap = RefCell::new(Box::new(pcap));

        Self {
            medium: phy::Medium::Ip,
            max_transmission_unit: mtu,
            pcap: Some(pcap),
            ..Default::default()
        }
    }

    #[inline]
    pub fn is_tun(&self) -> bool {
        self.medium == phy::Medium::Ip
    }

    #[inline]
    pub fn metrics(&self) -> ChannelMetrics {
        self.metrics.borrow().clone()
    }

    #[inline]
    pub fn phy_rx(&mut self, data: impl Into<Payload>) {
        self.rx_queue.push_back(data.into());
    }

    #[inline]
    pub fn next_phy_tx(&mut self) -> Option<Vec<u8>> {
        self.tx_queue.pop_front()
    }
}

impl phy::Device for CaptureDevice {
    type RxToken<'a> = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a> = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let item = self.rx_queue.pop_front();
        item.map(move |buffer| {
            let rx = RxToken {
                buffer,
                pcap: &self.pcap,
                metrics: self.metrics.clone(),
                timestamp,
            };
            let tx = TxToken {
                queue: &mut self.tx_queue,
                pcap: &self.pcap,
                metrics: self.metrics.clone(),
                timestamp,
            };
            (rx, tx)
        })
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            queue: &mut self.tx_queue,
            pcap: &self.pcap,
            metrics: self.metrics.clone(),
            timestamp,
        })
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = phy::DeviceCapabilities::default();
        caps.max_transmission_unit = self.max_transmission_unit;
        caps.medium = self.medium;
        caps
    }
}

/// Receipt token
pub struct RxToken<'a> {
    buffer: Payload,
    pcap: &'a Option<Pcap>,
    metrics: Rc<RefCell<ChannelMetrics>>,
    timestamp: Instant,
}

impl<'a> phy::RxToken for RxToken<'a> {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(self.buffer.as_mut());

        {
            let mut metrics = self.metrics.borrow_mut();
            metrics.rx.push(self.buffer.len() as f32);
        }

        if let Some(pcap) = self.pcap {
            pcap.borrow_mut()
                .packet(self.timestamp, self.buffer.as_ref());
        }

        result
    }
}

/// Transmission token
pub struct TxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
    pcap: &'a Option<Pcap>,
    metrics: Rc<RefCell<ChannelMetrics>>,
    timestamp: Instant,
}

impl<'a> phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0; len];
        buffer.resize(len, 0);
        let result = f(&mut buffer);

        {
            let mut metrics = self.metrics.borrow_mut();
            metrics.tx.push(buffer.len() as f32);
        }

        if let Some(pcap) = self.pcap {
            pcap.borrow_mut().packet(self.timestamp, buffer.as_ref());
        }

        self.queue.push_back(buffer);
        result
    }
}
