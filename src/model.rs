use std::net::IpAddr;

pub const UNKNOWN_DOMAIN: &str = "(unknown)";
pub const UNKNOWN_APPLICATION: &str = "(unknown application)";
pub const HISTORICAL_APPLICATION: &str = "(historical data)";
pub const DEFAULT_BPF_FILTER: &str = "tcp port 443 or udp port 443";
pub const TLS_PORT: u16 = 443;
pub const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 1;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_SNAPLEN: i32 = 65_535;
pub const DEFAULT_BUFFER_SIZE: i32 = 4_000_000;
pub const DEFAULT_TIMEOUT_MS: i32 = 250;
pub const MAX_TLS_RECORD_BODY: usize = 18_432;
pub const MAX_TLS_BUFFER: usize = 5 + MAX_TLS_RECORD_BODY;
pub const MAX_TRACKED_FLOWS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Endpoint {
    pub ip: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub protocol: TransportProtocol,
    pub first: Endpoint,
    pub second: Endpoint,
}

impl FlowKey {
    pub fn canonical(protocol: TransportProtocol, source: Endpoint, destination: Endpoint) -> Self {
        if source < destination {
            Self {
                protocol,
                first: source,
                second: destination,
            }
        } else {
            Self {
                protocol,
                first: destination,
                second: source,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpMetadata {
    pub sequence: u32,
    pub syn: bool,
    pub ack: bool,
    pub fin: bool,
    pub rst: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketObservation {
    pub timestamp_micros: i64,
    pub flow: FlowKey,
    pub source: Endpoint,
    pub destination: Endpoint,
    pub wire_len: u64,
    pub tcp: Option<TcpMetadata>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub bytes: u64,
    pub packets: u64,
}

impl Counters {
    pub fn add_saturating(&mut self, bytes: u64, packets: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.packets = self.packets.saturating_add(packets);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficBreakdown {
    pub upload_bytes: u64,
    pub download_bytes: u64,
    pub upload_packets: u64,
    pub download_packets: u64,
    pub tcp_bytes: u64,
    pub udp_bytes: u64,
    pub tcp_packets: u64,
    pub udp_packets: u64,
}

impl TrafficBreakdown {
    pub fn from_packet(wire_len: u64, upload: bool, protocol: TransportProtocol) -> Self {
        let mut value = Self::default();
        if upload {
            value.upload_bytes = wire_len;
            value.upload_packets = 1;
        } else {
            value.download_bytes = wire_len;
            value.download_packets = 1;
        }
        match protocol {
            TransportProtocol::Tcp => {
                value.tcp_bytes = wire_len;
                value.tcp_packets = 1;
            }
            TransportProtocol::Udp => {
                value.udp_bytes = wire_len;
                value.udp_packets = 1;
            }
        }
        value
    }

    pub fn add_saturating(&mut self, other: Self) {
        self.upload_bytes = self.upload_bytes.saturating_add(other.upload_bytes);
        self.download_bytes = self.download_bytes.saturating_add(other.download_bytes);
        self.upload_packets = self.upload_packets.saturating_add(other.upload_packets);
        self.download_packets = self
            .download_packets
            .saturating_add(other.download_packets);
        self.tcp_bytes = self.tcp_bytes.saturating_add(other.tcp_bytes);
        self.udp_bytes = self.udp_bytes.saturating_add(other.udp_bytes);
        self.tcp_packets = self.tcp_packets.saturating_add(other.tcp_packets);
        self.udp_packets = self.udp_packets.saturating_add(other.udp_packets);
    }

    pub fn bytes(self) -> u64 {
        self.upload_bytes.saturating_add(self.download_bytes)
    }

    pub fn packets(self) -> u64 {
        self.upload_packets.saturating_add(self.download_packets)
    }

    pub fn is_empty(self) -> bool {
        self.bytes() == 0 && self.packets() == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplicationCounters {
    pub counters: Counters,
    pub breakdown: TrafficBreakdown,
}

impl ApplicationCounters {
    pub fn add_saturating(&mut self, counters: Counters, breakdown: TrafficBreakdown) {
        self.counters
            .add_saturating(counters.bytes, counters.packets);
        self.breakdown.add_saturating(breakdown);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDelta {
    pub day_start_utc: i64,
    pub domain: String,
    pub counters: Counters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationDomainDelta {
    pub day_start_utc: i64,
    pub application: String,
    pub domain: String,
    pub counters: Counters,
    pub breakdown: TrafficBreakdown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlushBatch {
    pub rows: Vec<DomainDelta>,
}

impl FlushBatch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationFlushBatch {
    pub rows: Vec<ApplicationDomainDelta>,
}

impl ApplicationFlushBatch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopDomainRow {
    pub domain: String,
    pub bytes: u64,
    pub packets: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopDomainDetailRow {
    pub domain: String,
    pub bytes: u64,
    pub packets: u64,
    pub breakdown: TrafficBreakdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopApplicationRow {
    pub application: String,
    pub bytes: u64,
    pub packets: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficTotals {
    pub bytes: u64,
    pub packets: u64,
    pub unknown_domain_bytes: u64,
    pub unknown_application_bytes: u64,
    pub breakdown: TrafficBreakdown,
}

pub fn day_start_utc_from_micros(timestamp_micros: i64) -> i64 {
    timestamp_micros.div_euclid(1_000_000).div_euclid(86_400) * 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_flow_key_is_direction_independent() {
        let source = Endpoint {
            ip: IpAddr::from([10, 0, 0, 2]),
            port: 50000,
        };
        let destination = Endpoint {
            ip: IpAddr::from([93, 184, 216, 34]),
            port: 443,
        };

        let key1 = FlowKey::canonical(TransportProtocol::Tcp, source.clone(), destination.clone());
        let key2 = FlowKey::canonical(TransportProtocol::Tcp, destination, source);

        assert_eq!(key1, key2);
    }

    #[test]
    fn day_start_utc_handles_positive_and_negative_timestamps() {
        assert_eq!(day_start_utc_from_micros(86_400_000_001), 86_400);
        assert_eq!(day_start_utc_from_micros(-1), -86_400);
    }

    #[test]
    fn counters_use_saturating_addition() {
        let mut counters = Counters {
            bytes: u64::MAX - 1,
            packets: u64::MAX - 1,
        };
        counters.add_saturating(10, 10);
        assert_eq!(counters.bytes, u64::MAX);
        assert_eq!(counters.packets, u64::MAX);
    }

    #[test]
    fn traffic_breakdown_tracks_direction_and_transport() {
        let mut breakdown = TrafficBreakdown::from_packet(100, true, TransportProtocol::Tcp);
        breakdown.add_saturating(TrafficBreakdown::from_packet(
            250,
            false,
            TransportProtocol::Udp,
        ));
        assert_eq!(breakdown.upload_bytes, 100);
        assert_eq!(breakdown.download_bytes, 250);
        assert_eq!(breakdown.tcp_bytes, 100);
        assert_eq!(breakdown.udp_bytes, 250);
        assert_eq!(breakdown.bytes(), 350);
        assert_eq!(breakdown.packets(), 2);
    }
}
