use crate::model::{
    day_start_utc_from_micros, ApplicationCounters, ApplicationDomainDelta, Counters, Endpoint,
    FlowKey, PacketObservation, TrafficBreakdown, TransportProtocol, MAX_TRACKED_FLOWS, TLS_PORT,
    UNKNOWN_APPLICATION, UNKNOWN_DOMAIN,
};
use std::collections::{BTreeMap, HashMap};
use std::mem;

#[derive(Debug)]
struct ApplicationFlowState {
    client: Endpoint,
    application: String,
    domain: Option<String>,
    pending_by_day: BTreeMap<i64, ApplicationCounters>,
    last_seen_micros: i64,
    initial_syn_sequence: Option<u32>,
    client_fin: bool,
    server_fin: bool,
}

pub struct ApplicationTracker {
    idle_timeout_micros: i64,
    max_flows: usize,
    flows: HashMap<FlowKey, ApplicationFlowState>,
}

impl ApplicationTracker {
    pub fn new(idle_timeout_micros: i64) -> Self {
        Self {
            idle_timeout_micros: idle_timeout_micros.max(1),
            max_flows: MAX_TRACKED_FLOWS,
            flows: HashMap::new(),
        }
    }

    pub fn observe(
        &mut self,
        packet: &PacketObservation,
        resolved_domain: Option<&str>,
        application: &str,
    ) -> Vec<ApplicationDomainDelta> {
        let application = normalize_application(application);
        if packet.flow.protocol == TransportProtocol::Udp {
            let upload = packet.destination.port == TLS_PORT;
            return vec![packet_delta(
                packet,
                &application,
                UNKNOWN_DOMAIN,
                upload,
            )];
        }

        let mut deltas = Vec::new();
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
                append_unresolved(old_state, &mut deltas);
            }
        }

        if !self.flows.contains_key(&key) {
            if self.flows.len() >= self.max_flows {
                self.evict_oldest(&mut deltas);
            }
            self.flows
                .insert(key.clone(), new_state(packet, application.clone(), is_syn));
        }

        let mut close_flow = false;
        {
            let state = self
                .flows
                .get_mut(&key)
                .expect("application flow state must exist after insertion");
            state.last_seen_micros = state.last_seen_micros.max(packet.timestamp_micros);
            if state.application == UNKNOWN_APPLICATION && application != UNKNOWN_APPLICATION {
                state.application = application;
            }

            let is_client_to_server = packet.source == state.client;
            let packet_counters = application_counters(packet, is_client_to_server);
            if let Some(domain) = state.domain.clone() {
                deltas.push(ApplicationDomainDelta {
                    day_start_utc: day_start_utc_from_micros(packet.timestamp_micros),
                    application: state.application.clone(),
                    domain,
                    counters: packet_counters.counters,
                    breakdown: packet_counters.breakdown,
                });
            } else {
                let entry = state
                    .pending_by_day
                    .entry(day_start_utc_from_micros(packet.timestamp_micros))
                    .or_default();
                entry.add_saturating(packet_counters.counters, packet_counters.breakdown);

                if let Some(domain) = resolved_domain {
                    state.domain = Some(domain.to_string());
                    append_pending(state, domain, &mut deltas);
                }
            }

            if let Some(tcp) = packet.tcp.as_ref() {
                if tcp.rst {
                    close_flow = true;
                } else if tcp.fin {
                    if is_client_to_server {
                        state.client_fin = true;
                    } else {
                        state.server_fin = true;
                    }
                    close_flow = state.client_fin && state.server_fin;
                }
            }
        }

        if close_flow {
            if let Some(state) = self.flows.remove(&key) {
                append_unresolved(state, &mut deltas);
            }
        }

        deltas
    }

    pub fn expire_idle(&mut self, now_micros: i64) -> Vec<ApplicationDomainDelta> {
        let keys: Vec<FlowKey> = self
            .flows
            .iter()
            .filter(|(_, state)| {
                now_micros >= state.last_seen_micros
                    && state.last_seen_micros <= now_micros.saturating_sub(self.idle_timeout_micros)
            })
            .map(|(key, _)| key.clone())
            .collect();

        let mut deltas = Vec::new();
        for key in keys {
            if let Some(state) = self.flows.remove(&key) {
                append_unresolved(state, &mut deltas);
            }
        }
        deltas
    }

    pub fn drain_all(&mut self) -> Vec<ApplicationDomainDelta> {
        let mut deltas = Vec::new();
        for (_, state) in self.flows.drain() {
            append_unresolved(state, &mut deltas);
        }
        deltas
    }

    fn evict_oldest(&mut self, deltas: &mut Vec<ApplicationDomainDelta>) {
        let key = self
            .flows
            .iter()
            .min_by(|(left_key, left_state), (right_key, right_state)| {
                left_state
                    .last_seen_micros
                    .cmp(&right_state.last_seen_micros)
                    .then_with(|| format!("{left_key:?}").cmp(&format!("{right_key:?}")))
            })
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            if let Some(state) = self.flows.remove(&key) {
                append_unresolved(state, deltas);
            }
        }
    }
}

fn new_state(
    packet: &PacketObservation,
    application: String,
    is_syn: bool,
) -> ApplicationFlowState {
    let client = if packet.destination.port == TLS_PORT {
        packet.source.clone()
    } else if packet.source.port == TLS_PORT {
        packet.destination.clone()
    } else if is_syn {
        packet.source.clone()
    } else {
        packet.flow.first.clone()
    };

    ApplicationFlowState {
        client,
        application,
        domain: None,
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

fn normalize_application(application: &str) -> String {
    let application = application.trim();
    if application.is_empty() {
        UNKNOWN_APPLICATION.to_string()
    } else {
        application.to_string()
    }
}

fn application_counters(packet: &PacketObservation, upload: bool) -> ApplicationCounters {
    ApplicationCounters {
        counters: Counters {
            bytes: packet.wire_len,
            packets: 1,
        },
        breakdown: TrafficBreakdown::from_packet(packet.wire_len, upload, packet.flow.protocol),
    }
}

fn append_pending(
    state: &mut ApplicationFlowState,
    domain: &str,
    deltas: &mut Vec<ApplicationDomainDelta>,
) {
    for (day_start_utc, counters) in mem::take(&mut state.pending_by_day) {
        deltas.push(ApplicationDomainDelta {
            day_start_utc,
            application: state.application.clone(),
            domain: domain.to_string(),
            counters: counters.counters,
            breakdown: counters.breakdown,
        });
    }
}

fn append_unresolved(mut state: ApplicationFlowState, deltas: &mut Vec<ApplicationDomainDelta>) {
    if state.domain.is_some() {
        return;
    }
    append_pending(&mut state, UNKNOWN_DOMAIN, deltas);
}

fn packet_delta(
    packet: &PacketObservation,
    application: &str,
    domain: &str,
    upload: bool,
) -> ApplicationDomainDelta {
    let counters = application_counters(packet, upload);
    ApplicationDomainDelta {
        day_start_utc: day_start_utc_from_micros(packet.timestamp_micros),
        application: application.to_string(),
        domain: domain.to_string(),
        counters: counters.counters,
        breakdown: counters.breakdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TcpMetadata;
    use std::net::IpAddr;

    fn endpoint(ip: [u8; 4], port: u16) -> Endpoint {
        Endpoint {
            ip: IpAddr::from(ip),
            port,
        }
    }

    fn packet(
        timestamp_micros: i64,
        sequence: u32,
        syn: bool,
        rst: bool,
        reverse: bool,
    ) -> PacketObservation {
        let client = endpoint([10, 0, 0, 1], 50_000);
        let server = endpoint([93, 184, 216, 34], 443);
        let (source, destination) = if reverse {
            (server.clone(), client.clone())
        } else {
            (client.clone(), server.clone())
        };
        PacketObservation {
            timestamp_micros,
            flow: FlowKey::canonical(TransportProtocol::Tcp, client, server),
            source,
            destination,
            wire_len: 64,
            tcp: Some(TcpMetadata {
                sequence,
                syn,
                ack: reverse,
                fin: false,
                rst,
                payload: Vec::new(),
            }),
        }
    }

    #[test]
    fn pending_packets_move_to_resolved_domain_with_direction() {
        let mut tracker = ApplicationTracker::new(10_000_000);
        let first = packet(1_000_000, 1, true, false, false);
        assert!(tracker.observe(&first, None, "chrome.exe").is_empty());

        let second = packet(1_000_001, 2, false, false, true);
        let deltas = tracker.observe(&second, Some("example.com"), "chrome.exe");
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].application, "chrome.exe");
        assert_eq!(deltas[0].domain, "example.com");
        assert_eq!(deltas[0].counters.bytes, 128);
        assert_eq!(deltas[0].counters.packets, 2);
        assert_eq!(deltas[0].breakdown.upload_bytes, 64);
        assert_eq!(deltas[0].breakdown.download_bytes, 64);
        assert_eq!(deltas[0].breakdown.tcp_bytes, 128);
    }

    #[test]
    fn unresolved_rst_is_saved_under_application() {
        let mut tracker = ApplicationTracker::new(10_000_000);
        let first = packet(1_000_000, 1, true, false, false);
        tracker.observe(&first, None, "msedge.exe");
        let rst = packet(1_000_001, 2, false, true, true);
        let deltas = tracker.observe(&rst, None, "msedge.exe");
        assert_eq!(deltas[0].application, "msedge.exe");
        assert_eq!(deltas[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(deltas[0].counters.packets, 2);
    }

    #[test]
    fn known_application_upgrades_unknown_state() {
        let mut tracker = ApplicationTracker::new(10_000_000);
        let first = packet(1_000_000, 1, true, false, false);
        tracker.observe(&first, None, UNKNOWN_APPLICATION);
        let second = packet(1_000_001, 2, false, false, false);
        let deltas = tracker.observe(&second, Some("example.com"), "firefox.exe");
        assert_eq!(deltas[0].application, "firefox.exe");
    }

    #[test]
    fn udp_is_immediately_attributed_with_direction_and_protocol() {
        let source = endpoint([10, 0, 0, 1], 53_000);
        let destination = endpoint([1, 1, 1, 1], 443);
        let observation = PacketObservation {
            timestamp_micros: 1_000_000,
            flow: FlowKey::canonical(TransportProtocol::Udp, source.clone(), destination.clone()),
            source,
            destination,
            wire_len: 54,
            tcp: None,
        };
        let mut tracker = ApplicationTracker::new(10_000_000);
        let deltas = tracker.observe(&observation, None, "chrome.exe");
        assert_eq!(deltas[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(deltas[0].application, "chrome.exe");
        assert_eq!(deltas[0].breakdown.upload_bytes, 54);
        assert_eq!(deltas[0].breakdown.udp_bytes, 54);
    }
}
