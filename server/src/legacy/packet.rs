use chrono::{DateTime, Utc};

use super::server::Server;
use ya_relay_proto::codec::PacketKind;

impl Server {
    /// Drop packets that are older than
    pub fn drop_policy(&self, packet: &PacketKind, timestamp: DateTime<Utc>) -> bool {
        let age = Utc::now() - timestamp;
        if packet.is_forward() && age > self.config.drop_forward_packets_older {
            return true;
        }

        if packet.is_protocol_packet() && age > self.config.drop_packets_older {
            return true;
        }
        false
    }
}
