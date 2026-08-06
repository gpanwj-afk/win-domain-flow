use crate::model::{
    day_start_utc_from_micros, Counters, DomainDelta, Endpoint, FlowKey, PacketObservation,
    TcpMetadata, TransportProtocol, DEFAULT_IDLE_TIMEOUT_SECS, MAX_TLS_BUFFER, MAX_TRACKED_FLOWS,
    TLS_PORT, UNKNOWN_DOMAIN,
};
use crate::tls::{parse_client_hello_sni, TlsParseResult};
use std::collections::{BTreeMap, HashMap};
use std::mem;

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
            update.deltas.push(packet_delta(&packet, UNKNOWN_DOMAIN));
            return update;
        }

        let key = packet.flow.clone();
        let is_syn = packet.tcp.as_ref().is_some_and(|tcp| tcp.syn && !tcp.ack);
        let retransmitted_syn = is_syn
            && self.flows.get(&key).is_some_and(|state| {
                state.initial_syn_sequence.is_some()
                    && packet
                        .tcp
                        .as_ref()
                        .is_some_and(|tcp| state.initial_syn_sequence == Some(tcp.sequence))
            });

        if is_syn && !retransmitted_syn {
            if let Some(old_state) = self.flows.remove(&key) {
                append_unresolved(old_state, &mut update.deltas);
            }
        }

        if !self.flows.contains_key(&key) {
            if self.flows.len() >= self.config.max_flows {
                self.evict_oldest(&mut update);
            }
            self.flows
                .insert(key.clone(), new_flow_state(&packet, is_syn));
        }

        let mut close_flow = false;
        {
            let flow = self
                .flows
                .get_mut(&key)
                .expect("flow state must exist after insertion");
            flow.last_seen_micros = flow.last_seen_micros.max(packet.timestamp_micros);

            let is_client_to_server = packet.source == flow.client;

            if let Some(domain) = flow.domain.clone() {
                update.deltas.push(packet_delta(&packet, &domain));
            } else {
                let day = day_start_utc_from_micros(packet.timestamp_micros);
                flow.pending_by_day
                    .entry(day)
                    .or_default()
                    .add_saturating(packet.wire_len, 1);

                if is_client_to_server {
                    if let Some(tcp) = packet.tcp.as_ref() {
                        if !tcp.payload.is_empty() {
                            if let Some(domain) = inspect_tls(flow, tcp, self.config.max_tls_buffer)
                            {
                                resolve_domain(flow, domain, &mut update);
                            }
                        }
                    }
                }
            }

            if let Some(tcp) = packet.tcp.as_ref() {
                if tcp.rst {
                    close_flow = true;
                } else if tcp.fin {
                    if is_client_to_server {
                        flow.client_fin = true;
                    } else {
                        flow.server_fin = true;
                    }
                    close_flow = flow.client_fin && flow.server_fin;
                }
            }
        }

        if close_flow {
            let state = self
                .flows
                .remove(&key)
                .expect("flow state must exist when closing");
            append_unresolved(state, &mut update.deltas);
            update.evicted_flows = update.evicted_flows.saturating_add(1);
        }

        update
    }

    pub fn expire_idle(&mut self, now_micros: i64) -> Vec<DomainDelta> {
        let mut to_remove = Vec::new();
        for (key, state) in &self.flows {
            if now_micros >= state.last_seen_micros
                && state.last_seen_micros
                    <= now_micros.saturating_sub(self.config.idle_timeout_micros)
            {
                to_remove.push(key.clone());
            }
        }

        let mut deltas = Vec::new();
        for key in to_remove {
            if let Some(state) = self.flows.remove(&key) {
                append_unresolved(state, &mut deltas);
            }
        }
        deltas
    }

    pub fn drain_all(&mut self) -> Vec<DomainDelta> {
        let mut deltas = Vec::new();
        for (_, state) in self.flows.drain() {
            append_unresolved(state, &mut deltas);
        }
        deltas
    }

    pub fn active_flows(&self) -> usize {
        self.flows.len()
    }

    fn evict_oldest(&mut self, update: &mut FlowUpdate) {
        let oldest_key = self
            .flows
            .iter()
            .min_by(|(left_key, left_state), (right_key, right_state)| {
                left_state
                    .last_seen_micros
                    .cmp(&right_state.last_seen_micros)
                    .then_with(|| format!("{left_key:?}").cmp(&format!("{right_key:?}")))
            })
            .map(|(key, _)| key.clone());

        if let Some(key) = oldest_key {
            if let Some(state) = self.flows.remove(&key) {
                append_unresolved(state, &mut update.deltas);
                update.evicted_flows = update.evicted_flows.saturating_add(1);
            }
        }
    }
}

fn new_flow_state(packet: &PacketObservation, is_syn: bool) -> FlowState {
    let (client, server) = if packet.destination.port == TLS_PORT {
        (packet.source.clone(), packet.destination.clone())
    } else if packet.source.port == TLS_PORT {
        (packet.destination.clone(), packet.source.clone())
    } else if is_syn {
        (packet.source.clone(), packet.destination.clone())
    } else {
        (packet.flow.first.clone(), packet.flow.second.clone())
    };

    FlowState {
        client,
        server,
        domain: None,
        inspection: InspectionState::WaitingForStart,
        pending_by_day: BTreeMap::new(),
        last_seen_micros: packet.timestamp_micros,
        initial_syn_sequence: if is_syn {
            packet.tcp.as_ref().map(|tcp| tcp.sequence)
        } else {
            None
        },
        client_fin: false,
        server_fin: false,
    }
}

fn inspect_tls(flow: &mut FlowState, tcp: &TcpMetadata, max_tls_buffer: usize) -> Option<String> {
    let state = mem::replace(&mut flow.inspection, InspectionState::Finished);

    match state {
        InspectionState::WaitingForStart => {
            if tcp.payload.first().copied() != Some(0x16) || tcp.payload.len() > max_tls_buffer {
                return None;
            }

            let assembler = ClientHelloAssembler {
                expected_next_sequence: tcp
                    .sequence
                    .wrapping_add(u32::try_from(tcp.payload.len()).unwrap_or(u32::MAX)),
                bytes: tcp.payload.clone(),
            };
            evaluate_assembler(flow, assembler, max_tls_buffer)
        }
        InspectionState::Collecting(mut assembler) => {
            let sequence = tcp.sequence;
            if sequence == assembler.expected_next_sequence {
                let new_len = assembler.bytes.len().checked_add(tcp.payload.len())?;
                if new_len > max_tls_buffer {
                    return None;
                }

                assembler.bytes.extend_from_slice(&tcp.payload);
                assembler.expected_next_sequence =
                    sequence.wrapping_add(u32::try_from(tcp.payload.len()).unwrap_or(u32::MAX));
                evaluate_assembler(flow, assembler, max_tls_buffer)
            } else if seq_before(sequence, assembler.expected_next_sequence) {
                flow.inspection = InspectionState::Collecting(assembler);
                None
            } else {
                None
            }
        }
        InspectionState::Finished => None,
    }
}

fn evaluate_assembler(
    flow: &mut FlowState,
    assembler: ClientHelloAssembler,
    max_tls_buffer: usize,
) -> Option<String> {
    match parse_client_hello_sni(&assembler.bytes) {
        TlsParseResult::Sni(domain) => Some(domain),
        TlsParseResult::NeedMoreData { required_total } if required_total <= max_tls_buffer => {
            flow.inspection = InspectionState::Collecting(assembler);
            None
        }
        _ => None,
    }
}

fn resolve_domain(flow: &mut FlowState, domain: String, update: &mut FlowUpdate) {
    flow.domain = Some(domain.clone());
    flow.inspection = InspectionState::Finished;
    update.newly_resolved_domain = Some(domain.clone());

    for (day_start_utc, counters) in mem::take(&mut flow.pending_by_day) {
        update.deltas.push(DomainDelta {
            day_start_utc,
            domain: domain.clone(),
            counters,
        });
    }
}

fn append_unresolved(state: FlowState, deltas: &mut Vec<DomainDelta>) {
    if state.domain.is_some() {
        return;
    }

    for (day_start_utc, counters) in state.pending_by_day {
        deltas.push(DomainDelta {
            day_start_utc,
            domain: UNKNOWN_DOMAIN.to_string(),
            counters,
        });
    }
}

fn packet_delta(packet: &PacketObservation, domain: &str) -> DomainDelta {
    DomainDelta {
        day_start_utc: day_start_utc_from_micros(packet.timestamp_micros),
        domain: domain.to_string(),
        counters: Counters {
            bytes: packet.wire_len,
            packets: 1,
        },
    }
}

fn seq_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn endpoint(ip: [u8; 4], port: u16) -> Endpoint {
        Endpoint {
            ip: IpAddr::from(ip),
            port,
        }
    }

    fn endpoints(client_port: u16) -> (Endpoint, Endpoint, FlowKey) {
        let client = endpoint([10, 0, 0, 1], client_port);
        let server = endpoint([93, 184, 216, 34], 443);
        let flow = FlowKey::canonical(TransportProtocol::Tcp, client.clone(), server.clone());
        (client, server, flow)
    }

    #[allow(clippy::too_many_arguments)]
    fn tcp_packet(
        flow: &FlowKey,
        source: &Endpoint,
        destination: &Endpoint,
        timestamp_micros: i64,
        wire_len: u64,
        sequence: u32,
        syn: bool,
        ack: bool,
        fin: bool,
        rst: bool,
        payload: Vec<u8>,
    ) -> PacketObservation {
        PacketObservation {
            timestamp_micros,
            flow: flow.clone(),
            source: source.clone(),
            destination: destination.clone(),
            wire_len,
            tcp: Some(TcpMetadata {
                sequence,
                syn,
                ack,
                fin,
                rst,
                payload,
            }),
        }
    }

    fn syn(
        flow: &FlowKey,
        client: &Endpoint,
        server: &Endpoint,
        ts: i64,
        seq: u32,
    ) -> PacketObservation {
        tcp_packet(
            flow,
            client,
            server,
            ts,
            64,
            seq,
            true,
            false,
            false,
            false,
            Vec::new(),
        )
    }

    fn client_hello() -> Vec<u8> {
        vec![
            0x16, 0x03, 0x01, 0x00, 0x43, 0x01, 0x00, 0x00, 0x3f, 0x03, 0x03, 0, 1, 2, 3, 4, 5, 6,
            7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 31, 0x00, 0x00, 0x02, 0x13, 0x01, 0x01, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.',
            b'c', b'o', b'm',
        ]
    }

    fn resolve_example(
        tracker: &mut FlowTracker,
        client: &Endpoint,
        server: &Endpoint,
        flow: &FlowKey,
        base_ts: i64,
        base_seq: u32,
    ) -> FlowUpdate {
        tracker.observe(syn(flow, client, server, base_ts, base_seq));
        tracker.observe(tcp_packet(
            flow,
            client,
            server,
            base_ts + 1,
            126,
            base_seq.wrapping_add(1),
            false,
            true,
            false,
            false,
            client_hello(),
        ))
    }

    fn sum_counters(deltas: &[DomainDelta], domain: &str) -> Counters {
        let mut counters = Counters::default();
        for delta in deltas.iter().filter(|delta| delta.domain == domain) {
            counters.add_saturating(delta.counters.bytes, delta.counters.packets);
        }
        counters
    }

    #[test]
    fn resolves_single_segment_client_hello() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_000);

        let update = resolve_example(&mut tracker, &client, &server, &flow, 1_000_000, 1_000);

        assert_eq!(update.newly_resolved_domain.as_deref(), Some("example.com"));
        assert_eq!(
            sum_counters(&update.deltas, "example.com"),
            Counters {
                bytes: 190,
                packets: 2
            }
        );
        assert_eq!(tracker.active_flows(), 1);
    }

    #[test]
    fn resolves_fragmented_client_hello_and_releases_pending_bytes() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_001);
        let hello = client_hello();
        let (first, second) = hello.split_at(5);

        tracker.observe(syn(&flow, &client, &server, 2_000_000, 2_000));
        let first_update = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            2_000_001,
            94,
            2_001,
            false,
            true,
            false,
            false,
            first.to_vec(),
        ));
        assert!(first_update.newly_resolved_domain.is_none());

        let second_update = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            2_000_002,
            86,
            2_006,
            false,
            true,
            false,
            false,
            second.to_vec(),
        ));

        assert_eq!(
            second_update.newly_resolved_domain.as_deref(),
            Some("example.com")
        );
        assert_eq!(
            sum_counters(&second_update.deltas, "example.com"),
            Counters {
                bytes: 244,
                packets: 3
            }
        );
    }

    #[test]
    fn retransmitted_payload_counts_wire_bytes_without_duplicate_assembly() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_002);
        let hello = client_hello();
        let (first, second) = hello.split_at(5);

        tracker.observe(syn(&flow, &client, &server, 3_000_000, 3_000));
        tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            3_000_001,
            94,
            3_001,
            false,
            true,
            false,
            false,
            first.to_vec(),
        ));
        tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            3_000_002,
            94,
            3_001,
            false,
            true,
            false,
            false,
            first.to_vec(),
        ));
        let update = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            3_000_003,
            86,
            3_006,
            false,
            true,
            false,
            false,
            second.to_vec(),
        ));

        assert_eq!(update.newly_resolved_domain.as_deref(), Some("example.com"));
        assert_eq!(
            sum_counters(&update.deltas, "example.com"),
            Counters {
                bytes: 338,
                packets: 4
            }
        );
    }

    #[test]
    fn retransmitted_syn_is_counted_without_resetting_state() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_003);

        tracker.observe(syn(&flow, &client, &server, 4_000_000, 4_000));
        let retransmit = tracker.observe(syn(&flow, &client, &server, 4_000_001, 4_000));
        assert!(retransmit.deltas.is_empty());
        assert_eq!(tracker.active_flows(), 1);

        let update = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            4_000_002,
            126,
            4_001,
            false,
            true,
            false,
            false,
            client_hello(),
        ));

        assert_eq!(
            sum_counters(&update.deltas, "example.com"),
            Counters {
                bytes: 254,
                packets: 3
            }
        );
    }

    #[test]
    fn sequence_gap_finishes_inspection_and_drains_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_004);

        tracker.observe(syn(&flow, &client, &server, 5_000_000, 5_000));
        tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            5_000_001,
            94,
            5_001,
            false,
            true,
            false,
            false,
            vec![0x16, 0x03, 0x01, 0x00, 0x43],
        ));
        tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            5_000_002,
            80,
            5_020,
            false,
            true,
            false,
            false,
            vec![0x01, 0x02, 0x03],
        ));

        let deltas = tracker.drain_all();
        assert_eq!(
            sum_counters(&deltas, UNKNOWN_DOMAIN),
            Counters {
                bytes: 238,
                packets: 3
            }
        );
    }

    #[test]
    fn unresolved_rst_becomes_unknown_and_removes_flow() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_005);

        tracker.observe(syn(&flow, &client, &server, 6_000_000, 6_000));
        tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            6_000_001,
            94,
            6_001,
            false,
            true,
            false,
            false,
            b"not tls".to_vec(),
        ));
        let update = tracker.observe(tcp_packet(
            &flow,
            &server,
            &client,
            6_000_002,
            54,
            9_000,
            false,
            true,
            false,
            true,
            Vec::new(),
        ));

        assert_eq!(tracker.active_flows(), 0);
        assert_eq!(update.evicted_flows, 1);
        assert_eq!(
            sum_counters(&update.deltas, UNKNOWN_DOMAIN),
            Counters {
                bytes: 212,
                packets: 3
            }
        );
    }

    #[test]
    fn udp_is_immediately_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let source = endpoint([10, 0, 0, 1], 50_006);
        let destination = endpoint([93, 184, 216, 34], 443);
        let flow = FlowKey::canonical(TransportProtocol::Udp, source.clone(), destination.clone());

        let update = tracker.observe(PacketObservation {
            timestamp_micros: 7_000_000,
            flow,
            source,
            destination,
            wire_len: 54,
            tcp: None,
        });

        assert_eq!(update.deltas.len(), 1);
        assert_eq!(update.deltas[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(
            update.deltas[0].counters,
            Counters {
                bytes: 54,
                packets: 1
            }
        );
        assert_eq!(tracker.active_flows(), 0);
    }

    #[test]
    fn tcp_and_udp_same_tuple_do_not_share_attribution() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, tcp_flow) = endpoints(50_007);
        let resolved = resolve_example(&mut tracker, &client, &server, &tcp_flow, 8_000_000, 8_000);
        assert_eq!(
            resolved.newly_resolved_domain.as_deref(),
            Some("example.com")
        );

        let udp_flow = FlowKey::canonical(TransportProtocol::Udp, client.clone(), server.clone());
        let update = tracker.observe(PacketObservation {
            timestamp_micros: 8_000_002,
            flow: udp_flow,
            source: client,
            destination: server,
            wire_len: 54,
            tcp: None,
        });

        assert_eq!(update.deltas[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(update.deltas[0].counters.bytes, 54);
        assert_eq!(tracker.active_flows(), 1);
    }

    #[test]
    fn idle_expiration_releases_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig {
            idle_timeout_micros: 300_000_000,
            ..FlowTrackerConfig::default()
        });
        let (client, server, flow) = endpoints(50_008);

        tracker.observe(syn(&flow, &client, &server, 1_000_000, 10_000));
        let deltas = tracker.expire_idle(301_000_000);

        assert_eq!(tracker.active_flows(), 0);
        assert_eq!(
            sum_counters(&deltas, UNKNOWN_DOMAIN),
            Counters {
                bytes: 64,
                packets: 1
            }
        );
    }

    #[test]
    fn resolved_rst_counts_packet_and_removes_flow() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_009);
        resolve_example(&mut tracker, &client, &server, &flow, 9_000_000, 9_000);

        let update = tracker.observe(tcp_packet(
            &flow,
            &server,
            &client,
            9_000_002,
            54,
            20_000,
            false,
            true,
            false,
            true,
            Vec::new(),
        ));

        assert_eq!(
            sum_counters(&update.deltas, "example.com"),
            Counters {
                bytes: 54,
                packets: 1
            }
        );
        assert_eq!(update.evicted_flows, 1);
        assert_eq!(tracker.active_flows(), 0);
    }

    #[test]
    fn resolved_bidirectional_fin_removes_flow() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_010);
        resolve_example(&mut tracker, &client, &server, &flow, 10_000_000, 10_000);

        let first_fin = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            10_000_002,
            54,
            11_000,
            false,
            true,
            true,
            false,
            Vec::new(),
        ));
        assert_eq!(
            sum_counters(&first_fin.deltas, "example.com"),
            Counters {
                bytes: 54,
                packets: 1
            }
        );
        assert_eq!(tracker.active_flows(), 1);

        let second_fin = tracker.observe(tcp_packet(
            &flow,
            &server,
            &client,
            10_000_003,
            54,
            21_000,
            false,
            true,
            true,
            false,
            Vec::new(),
        ));
        assert_eq!(
            sum_counters(&second_fin.deltas, "example.com"),
            Counters {
                bytes: 54,
                packets: 1
            }
        );
        assert_eq!(second_fin.evicted_flows, 1);
        assert_eq!(tracker.active_flows(), 0);
    }

    #[test]
    fn new_syn_with_different_sequence_releases_old_unknown() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_011);

        tracker.observe(syn(&flow, &client, &server, 11_000_000, 1_000));
        let update = tracker.observe(syn(&flow, &client, &server, 11_000_001, 2_000));

        assert_eq!(
            sum_counters(&update.deltas, UNKNOWN_DOMAIN),
            Counters {
                bytes: 64,
                packets: 1
            }
        );
        assert_eq!(tracker.active_flows(), 1);

        let remaining = tracker.drain_all();
        assert_eq!(
            sum_counters(&remaining, UNKNOWN_DOMAIN),
            Counters {
                bytes: 64,
                packets: 1
            }
        );
    }

    #[test]
    fn resolution_preserves_utc_day_buckets() {
        let mut tracker = FlowTracker::new(FlowTrackerConfig::default());
        let (client, server, flow) = endpoints(50_012);
        let day_end = 86_400_000_000 - 1;

        tracker.observe(syn(&flow, &client, &server, day_end, 30_000));
        let update = tracker.observe(tcp_packet(
            &flow,
            &client,
            &server,
            86_400_000_001,
            126,
            30_001,
            false,
            true,
            false,
            false,
            client_hello(),
        ));

        assert_eq!(update.deltas.len(), 2);
        assert_eq!(update.deltas[0].day_start_utc, 0);
        assert_eq!(
            update.deltas[0].counters,
            Counters {
                bytes: 64,
                packets: 1
            }
        );
        assert_eq!(update.deltas[1].day_start_utc, 86_400);
        assert_eq!(
            update.deltas[1].counters,
            Counters {
                bytes: 126,
                packets: 1
            }
        );
        assert!(update
            .deltas
            .iter()
            .all(|delta| delta.domain == "example.com"));
    }
}
