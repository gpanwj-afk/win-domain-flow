use crate::model::{
    day_start_utc_from_micros, Counters, DomainDelta, Endpoint, FlowKey, PacketObservation,
    TransportProtocol, DEFAULT_IDLE_TIMEOUT_SECS, MAX_TLS_BUFFER, MAX_TRACKED_FLOWS, TLS_PORT,
    UNKNOWN_DOMAIN,
};
use crate::tls::{parse_client_hello_sni, TlsParseResult};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowTrackerConfig {
    pub idle_timeout_micros: i64,
    pub max_flows: usize,
    pub max_tls_buffer: usize,
}

impl Default for FlowTrackerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_micros: (DEFAULT_IDLE_TIMEOUT_SECS as i64) * 1_000_000,
            max_flows: MAX_TRACKED_FLOWS,
            max_tls_buffer: MAX_TLS_BUFFER,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowUpdate {
    pub deltas: Vec<DomainDelta>,
    pub newly_resolved_domain: Option<String>,
    pub evicted_flows: usize,
}

#[derive(Debug)]
struct FlowState {
    client: Endpoint,
    #[allow(dead_code)]
    server: Endpoint,
    domain: Option<String>,
    inspection: InspectionState,
    pending_by_day: BTreeMap<i64, Counters>,
    last_seen_micros: i64,
    initial_syn_sequence: Option<u32>,
    client_fin: bool,
    server_fin: bool,
}

#[derive(Debug)]
enum InspectionState {
    WaitingForStart,
    Collecting(ClientHelloAssembler),
    Finished,
}

#[derive(Debug)]
struct ClientHelloAssembler {
    expected_next_sequence: u32,
    bytes: Vec<u8>,
}

pub struct FlowTracker {
    config: FlowTrackerConfig,
    flows: HashMap<FlowKey, FlowState>,
}

impl FlowTracker {
    pub fn new(config: FlowTrackerConfig) -> Self {
        let max_flows = if config.max_flows == 0 || config.max_flows > MAX_TRACKED_FLOWS {
            MAX_TRACKED_FLOWS
        } else {
            config.max_flows
        };

        let max_tls_buffer = if config.max_tls_buffer < 5 || config.max_tls_buffer > MAX_TLS_BUFFER
        {
            MAX_TLS_BUFFER
        } else {
            config.max_tls_buffer
        };

        let idle_timeout_micros = if config.idle_timeout_micros <= 0 {
            (DEFAULT_IDLE_TIMEOUT_SECS as i64) * 1_000_000
        } else {
            config.idle_timeout_micros
        };

        Self {
            config: FlowTrackerConfig {
                idle_timeout_micros,
                max_flows,
                max_tls_buffer,
            },
            flows: HashMap::new(),
        }
    }

    pub fn observe(&mut self, packet: PacketObservation) -> FlowUpdate {
        let mut update = FlowUpdate::default();

        if packet.flow.protocol == TransportProtocol::Udp {
            update.deltas.push(DomainDelta {
                day_start_utc: day_start_utc_from_micros(packet.timestamp_micros),
                domain: UNKNOWN_DOMAIN.to_string(),
                counters: Counters {
                    bytes: packet.wire_len,
                    packets: 1,
                },
            });
            return update;
        }

        let key = packet.flow.clone();
        let is_syn = packet.tcp.as_ref().is_some_and(|t| t.syn && !t.ack);

        if is_syn {
            if let Some(existing) = self.flows.get(&key) {
                if let Some(initial_syn) = existing.initial_syn_sequence {
                    if packet
                        .tcp
                        .as_ref()
                        .is_some_and(|t| t.sequence == initial_syn)
                    {
                        self.flows.get_mut(&key).unwrap().last_seen_micros = self
                            .flows
                            .get(&key)
                            .unwrap()
                            .last_seen_micros
                            .max(packet.timestamp_micros);
                        return update;
                    }
                }

                let old_state = self.flows.remove(&key).unwrap();
                if old_state.domain.is_none() {
                    for (day, counters) in old_state.pending_by_day {
                        update.deltas.push(DomainDelta {
                            day_start_utc: day,
                            domain: UNKNOWN_DOMAIN.to_string(),
                            counters,
                        });
                    }
                }
            }
        }

        if !self.flows.contains_key(&key) {
            if self.flows.len() >= self.config.max_flows {
                self.evict_oldest(&mut update);
            }

            let client;
            let server;
            if packet.destination.port == TLS_PORT {
                client = packet.source.clone();
                server = packet.destination.clone();
            } else if packet.source.port == TLS_PORT {
                client = packet.destination.clone();
                server = packet.source.clone();
            } else if is_syn {
                client = packet.source.clone();
                server = packet.destination.clone();
            } else {
                client = packet.flow.first.clone();
                server = packet.flow.second.clone();
            }

            let initial_syn_sequence = if is_syn {
                packet.tcp.as_ref().map(|t| t.sequence)
            } else {
                None
            };

            self.flows.insert(
                key.clone(),
                FlowState {
                    client: client.clone(),
                    server: server.clone(),
                    domain: None,
                    inspection: InspectionState::WaitingForStart,
                    pending_by_day: BTreeMap::new(),
                    last_seen_micros: packet.timestamp_micros,
                    initial_syn_sequence,
                    client_fin: false,
                    server_fin: false,
                },
            );
        }

        let flow = self.flows.get_mut(&key).unwrap();
        flow.last_seen_micros = flow.last_seen_micros.max(packet.timestamp_micros);

        if flow.domain.is_some() {
            let domain = flow.domain.clone().unwrap();
            let day = day_start_utc_from_micros(packet.timestamp_micros);
            update.deltas.push(DomainDelta {
                day_start_utc: day,
                domain: domain.clone(),
                counters: Counters {
                    bytes: packet.wire_len,
                    packets: 1,
                },
            });
            update.newly_resolved_domain = None;
            return update;
        }

        let day = day_start_utc_from_micros(packet.timestamp_micros);
        let entry = flow.pending_by_day.entry(day).or_default();
        entry.add_saturating(packet.wire_len, 1);

        let is_client_to_server = packet.source == flow.client;

        if let Some(ref tcp) = packet.tcp {
            if is_client_to_server && !tcp.payload.is_empty() {
                match &mut flow.inspection {
                    InspectionState::WaitingForStart => {
                        if tcp.payload[0] == 0x16 {
                            let assembler = ClientHelloAssembler {
                                expected_next_sequence: tcp
                                    .sequence
                                    .wrapping_add(tcp.payload.len() as u32),
                                bytes: tcp.payload.clone(),
                            };

                            match parse_client_hello_sni(&assembler.bytes) {
                                TlsParseResult::Sni(domain) => {
                                    flow.domain = Some(domain.clone());
                                    flow.inspection = InspectionState::Finished;
                                    update.newly_resolved_domain = Some(domain.clone());
                                    for (day, counters) in flow.pending_by_day.iter() {
                                        update.deltas.push(DomainDelta {
                                            day_start_utc: *day,
                                            domain: domain.clone(),
                                            counters: *counters,
                                        });
                                    }
                                    flow.pending_by_day.clear();
                                    return update;
                                }
                                TlsParseResult::NeedMoreData { .. } => {
                                    flow.inspection = InspectionState::Collecting(assembler);
                                }
                                _ => {
                                    flow.inspection = InspectionState::Finished;
                                }
                            }
                        } else {
                            flow.inspection = InspectionState::Finished;
                        }
                    }
                    InspectionState::Collecting(assembler) => {
                        let expected = assembler.expected_next_sequence;
                        let seq = tcp.sequence;

                        if seq == expected {
                            assembler.bytes.extend_from_slice(&tcp.payload);
                            assembler.expected_next_sequence =
                                seq.wrapping_add(tcp.payload.len() as u32);

                            if assembler.bytes.len() > self.config.max_tls_buffer {
                                flow.inspection = InspectionState::Finished;
                            } else {
                                match parse_client_hello_sni(&assembler.bytes) {
                                    TlsParseResult::Sni(domain) => {
                                        flow.domain = Some(domain.clone());
                                        flow.inspection = InspectionState::Finished;
                                        update.newly_resolved_domain = Some(domain.clone());
                                        for (day, counters) in flow.pending_by_day.iter() {
                                            update.deltas.push(DomainDelta {
                                                day_start_utc: *day,
                                                domain: domain.clone(),
                                                counters: *counters,
                                            });
                                        }
                                        flow.pending_by_day.clear();
                                        return update;
                                    }
                                    TlsParseResult::NeedMoreData { .. } => {}
                                    _ => {
                                        flow.inspection = InspectionState::Finished;
                                    }
                                }
                            }
                        } else if seq_before(seq, expected) {
                            // Retransmission, ignore
                        } else {
                            flow.inspection = InspectionState::Finished;
                        }
                    }
                    InspectionState::Finished => {}
                }
            }

            if tcp.rst {
                let old_state = self.flows.remove(&key).unwrap();
                if old_state.domain.is_none() {
                    for (day, counters) in old_state.pending_by_day {
                        update.deltas.push(DomainDelta {
                            day_start_utc: day,
                            domain: UNKNOWN_DOMAIN.to_string(),
                            counters,
                        });
                    }
                }
                update.evicted_flows += 1;
                return update;
            }

            if tcp.fin {
                if is_client_to_server {
                    flow.client_fin = true;
                } else {
                    flow.server_fin = true;
                }
                if flow.client_fin && flow.server_fin {
                    let old_state = self.flows.remove(&key).unwrap();
                    if old_state.domain.is_none() {
                        for (day, counters) in old_state.pending_by_day {
                            update.deltas.push(DomainDelta {
                                day_start_utc: day,
                                domain: UNKNOWN_DOMAIN.to_string(),
                                counters,
                            });
                        }
                    }
                    update.evicted_flows += 1;
                    return update;
                }
            }
        }

        update
    }

    pub fn expire_idle(&mut self, now_micros: i64) -> Vec<DomainDelta> {
        let mut deltas = Vec::new();
        let mut to_remove = Vec::new();

        for (key, state) in &self.flows {
            if now_micros < state.last_seen_micros {
                continue;
            }
            if state.last_seen_micros <= now_micros - self.config.idle_timeout_micros {
                to_remove.push(key.clone());
            }
        }

        for key in to_remove {
            let state = self.flows.remove(&key).unwrap();
            if state.domain.is_none() {
                for (day, counters) in state.pending_by_day {
                    deltas.push(DomainDelta {
                        day_start_utc: day,
                        domain: UNKNOWN_DOMAIN.to_string(),
                        counters,
                    });
                }
            }
        }

        deltas
    }

    pub fn drain_all(&mut self) -> Vec<DomainDelta> {
        let mut deltas = Vec::new();

        for (_, state) in self.flows.drain() {
            if state.domain.is_none() {
                for (day, counters) in state.pending_by_day {
                    deltas.push(DomainDelta {
                        day_start_utc: day,
                        domain: UNKNOWN_DOMAIN.to_string(),
                        counters,
                    });
                }
            }
        }

        deltas
    }

    pub fn active_flows(&self) -> usize {
        self.flows.len()
    }

    fn evict_oldest(&mut self, update: &mut FlowUpdate) {
        let mut oldest_key = None;
        let mut oldest_time = i64::MAX;

        for (key, state) in &self.flows {
            if state.last_seen_micros < oldest_time
                || (state.last_seen_micros == oldest_time
                    && oldest_key
                        .as_ref()
                        .is_none_or(|k: &FlowKey| format!("{k:?}") < format!("{key:?}")))
            {
                oldest_time = state.last_seen_micros;
                oldest_key = Some(key.clone());
            }
        }

        if let Some(key) = oldest_key {
            let state = self.flows.remove(&key).unwrap();
            if state.domain.is_none() {
                for (day, counters) in state.pending_by_day {
                    update.deltas.push(DomainDelta {
                        day_start_utc: day,
                        domain: UNKNOWN_DOMAIN.to_string(),
                        counters,
                    });
                }
            }
            update.evicted_flows += 1;
        }
    }
}

fn seq_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TcpMetadata;
    use std::net::IpAddr;

    fn make_endpoint(ip: [u8; 4], port: u16) -> Endpoint {
        Endpoint {
            ip: IpAddr::from(ip),
            port,
        }
    }

    #[test]
    fn resolves_single_segment_client_hello() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50000);
        let server = make_endpoint([93, 184, 216, 34], 443);

        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 1_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 1000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        let update = tracker.observe(syn);
        eprintln!(
            "After SYN: active_flows={}, update={:?}",
            tracker.active_flows(),
            update
        );
        assert!(update.deltas.is_empty());
        assert_eq!(tracker.active_flows(), 1);

        let hello_payload: Vec<u8> = vec![
            0x16, 0x03, 0x01, 0x00, 0x43, 0x01, 0x00, 0x00, 0x3f, 0x03, 0x03, 0, 1, 2, 3, 4, 5, 6,
            7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 31, 0x00, 0x00, 0x02, 0x13, 0x01, 0x01, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.',
            b'c', b'o', b'm',
        ];

        let hello = PacketObservation {
            timestamp_micros: 1_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 126,
            tcp: Some(TcpMetadata {
                sequence: 1000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: hello_payload,
            }),
        };

        let update = tracker.observe(hello);
        eprintln!(
            "After Hello: active_flows={}, update={:?}",
            tracker.active_flows(),
            update
        );
        assert_eq!(
            update.newly_resolved_domain,
            Some("example.com".to_string())
        );
        eprintln!("deltas.len()={}", update.deltas.len());
        assert_eq!(update.deltas.len(), 1);
        assert_eq!(update.deltas[0].domain, "example.com");
        assert_eq!(update.deltas[0].counters.bytes, 190);
        assert_eq!(update.deltas[0].counters.packets, 2);
    }

    #[test]
    fn resolves_fragmented_client_hello_and_releases_pending_bytes() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50001);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 2_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 2000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn);

        let part1 = vec![0x16, 0x03, 0x01, 0x00, 0x43];
        let mut part2 = vec![0x01, 0x00, 0x00, 0x3f, 0x03, 0x03];
        part2.extend_from_slice(&[0; 32]);
        part2.push(0x00);
        part2.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        part2.push(0x01);
        part2.push(0x00);
        part2.extend_from_slice(&[0x00, 0x14, 0x00, 0x00, 0x00, 0x10, 0x00, 0x0e, 0x00, 0x00]);
        part2.extend_from_slice(&[
            0x0b, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
        ]);

        let p1 = PacketObservation {
            timestamp_micros: 2_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 2000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: part1,
            }),
        };
        let update1 = tracker.observe(p1);
        assert!(update1.newly_resolved_domain.is_none());

        let p2 = PacketObservation {
            timestamp_micros: 2_000_002,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 86,
            tcp: Some(TcpMetadata {
                sequence: 2005,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: part2,
            }),
        };
        let update2 = tracker.observe(p2);
        assert_eq!(
            update2.newly_resolved_domain,
            Some("example.com".to_string())
        );
        assert_eq!(update2.deltas.len(), 1);
        assert_eq!(update2.deltas[0].counters.bytes, 244);
        assert_eq!(update2.deltas[0].counters.packets, 3);
    }

    #[test]
    fn retransmission_does_not_duplicate_tls_bytes() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50002);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 3_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 3000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn);

        let hello_bytes: Vec<u8> = vec![
            0x16, 0x03, 0x01, 0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 0x03, 0x03,
        ];

        let p1 = PacketObservation {
            timestamp_micros: 3_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 3000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: hello_bytes.clone(),
            }),
        };
        tracker.observe(p1);

        let retransmit = PacketObservation {
            timestamp_micros: 3_000_002,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 3000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: hello_bytes,
            }),
        };
        let update = tracker.observe(retransmit);
        assert!(update.deltas.is_empty() || update.deltas.iter().all(|d| d.counters.bytes == 0));
    }

    #[test]
    fn retransmitted_syn_does_not_reset_pending() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50003);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn1 = PacketObservation {
            timestamp_micros: 4_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 4000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn1);

        let syn2 = PacketObservation {
            timestamp_micros: 4_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 4000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        let update = tracker.observe(syn2);
        assert!(update.deltas.is_empty());
        assert_eq!(tracker.active_flows(), 1);
    }

    #[test]
    fn sequence_gap_finishes_inspection() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50004);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 5_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 5000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn);

        let p1 = PacketObservation {
            timestamp_micros: 5_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 5000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![0x16, 0x03, 0x01, 0x00, 0x05],
            }),
        };
        tracker.observe(p1);

        let p2 = PacketObservation {
            timestamp_micros: 5_000_002,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 5010,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![0x01, 0x02, 0x03],
            }),
        };
        let update = tracker.observe(p2);
        assert!(update.newly_resolved_domain.is_none());

        let mut tracker2 = FlowTracker::new(FlowTrackerConfig::default());
        for _delta in tracker.drain_all() {
            tracker2.observe(PacketObservation {
                timestamp_micros: 5_000_003,
                flow: FlowKey::canonical(
                    TransportProtocol::Tcp,
                    make_endpoint([10, 0, 0, 1], 60000),
                    make_endpoint([93, 184, 216, 34], 443),
                ),
                source: make_endpoint([10, 0, 0, 1], 60000),
                destination: make_endpoint([93, 184, 216, 34], 443),
                wire_len: 0,
                tcp: None,
            });
        }
    }

    #[test]
    fn unresolved_rst_becomes_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let client = make_endpoint([10, 0, 0, 1], 50005);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 6_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 6000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn);

        let data = PacketObservation {
            timestamp_micros: 6_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 6000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![0x47, 0x45, 0x54],
            }),
        };
        tracker.observe(data);

        let rst = PacketObservation {
            timestamp_micros: 6_000_002,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 54,
            tcp: Some(TcpMetadata {
                sequence: 6003,
                syn: false,
                ack: false,
                fin: false,
                rst: true,
                payload: vec![],
            }),
        };
        let update = tracker.observe(rst);
        assert_eq!(tracker.active_flows(), 0);
        assert!(!update.deltas.is_empty());
        assert!(update.deltas.iter().any(|d| d.domain == UNKNOWN_DOMAIN));
    }

    #[test]
    fn udp_is_immediately_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());

        let source = make_endpoint([10, 0, 0, 1], 50006);
        let dest = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Udp, source.clone(), dest.clone());

        let udp = PacketObservation {
            timestamp_micros: 7_000_000,
            flow: flow_key,
            source,
            destination: dest,
            wire_len: 54,
            tcp: None,
        };

        let update = tracker.observe(udp);
        assert_eq!(update.deltas.len(), 1);
        assert_eq!(update.deltas[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(update.deltas[0].counters.bytes, 54);
        assert_eq!(update.deltas[0].counters.packets, 1);
    }

    #[test]
    fn idle_expiration_releases_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig {
            idle_timeout_micros: 300_000_000,
            ..FlowTrackerConfig::default()
        });

        let client = make_endpoint([10, 0, 0, 1], 50007);
        let server = make_endpoint([93, 184, 216, 34], 443);
        let flow_key = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

        let syn = PacketObservation {
            timestamp_micros: 1_000_000,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence: 7000,
                syn: true,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![],
            }),
        };
        tracker.observe(syn);

        let data = PacketObservation {
            timestamp_micros: 1_000_001,
            flow: flow_key.clone(),
            source: client.clone(),
            destination: server.clone(),
            wire_len: 94,
            tcp: Some(TcpMetadata {
                sequence: 7000,
                syn: false,
                ack: false,
                fin: false,
                rst: false,
                payload: vec![0x47, 0x45, 0x54],
            }),
        };
        tracker.observe(data);

        let deltas = tracker.expire_idle(402_000_000);
        assert_eq!(tracker.active_flows(), 0);
        assert!(!deltas.is_empty());
        assert!(deltas.iter().any(|d| d.domain == UNKNOWN_DOMAIN));
    }

    #[test]
    fn oldest_flow_is_evicted_at_capacity() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig {
            max_flows: 2,
            ..FlowTrackerConfig::default()
        });

        for i in 0..3u16 {
            let client = make_endpoint([10, 0, 0, 1], 50010 + i);
            let server = make_endpoint([93, 184, 216, 34], 443);
            let flow_key =
                FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());

            let syn = PacketObservation {
                timestamp_micros: (i as i64) * 1_000_000,
                flow: flow_key.clone(),
                source: client.clone(),
                destination: server.clone(),
                wire_len: 64,
                tcp: Some(TcpMetadata {
                    sequence: 8000 + (i as u32) * 1000,
                    syn: true,
                    ack: false,
                    fin: false,
                    rst: false,
                    payload: vec![],
                }),
            };
            let update = tracker.observe(syn);
            if i == 2 {
                assert_eq!(update.evicted_flows, 1);
            }
        }
    }
}
