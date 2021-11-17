use smoltcp::phy;
use smoltcp::time;
use std::collections::VecDeque;

const MTU: usize = 256000;

/// Network device capable of injecting and extracting packets
#[derive(Clone, Default)]
pub struct CaptureDevice {
    tx_queue: VecDeque<Vec<u8>>,
    rx_queue: VecDeque<Vec<u8>>,
}

impl CaptureDevice {
    pub fn phy_rx(&mut self, data: Vec<u8>) {
        self.rx_queue.push_back(data);
    }

    pub fn next_phy_tx(&mut self) -> Option<Vec<u8>> {
        self.tx_queue.pop_front()
    }
}

impl<'a> phy::Device<'a> for CaptureDevice {
    type RxToken = RxToken;
    type TxToken = TxToken<'a>;

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let item = self.rx_queue.pop_front();
        item.map(move |buffer| {
            let rx = RxToken { buffer };
            let tx = TxToken {
                queue: &mut self.tx_queue,
            };
            (rx, tx)
        })
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(TxToken {
            queue: &mut self.tx_queue,
        })
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = phy::DeviceCapabilities::default();
        caps.max_transmission_unit = MTU;
        caps
    }
}

/// Receipt token
pub struct RxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(mut self, _timestamp: time::Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        f(&mut self.buffer)
    }
}

/// Transmission token
pub struct TxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
}

impl<'a> phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, _timestamp: time::Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let mut buffer = vec![0; len];
        buffer.resize(len, 0);
        let result = f(&mut buffer);
        if result.is_ok() {
            self.queue.push_back(buffer);
        }
        result
    }
}
