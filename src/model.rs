use std::net::IpAddr;

pub const UNKNOWN_DOMAIN: &str = "(unknown)";
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDelta {
    pub day_start_utc: i64,
    pub domain: String,
    pub counters: Counters,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopDomainRow {
    pub domain: String,
    pub bytes: u64,
    pub packets: u64,
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
}
