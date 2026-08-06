use crate::capture::OwnedPacket;
use crate::model::{
    Endpoint, FlowKey, PacketObservation, TcpMetadata, TransportProtocol, MAX_TLS_BUFFER, TLS_PORT,
};
use etherparse::{LaxPacketHeaders, LaxPayloadSlice, NetHeaders, TransportHeader};
use pcap::Linktype;
use std::net::IpAddr;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PacketParseError {
    #[error("unsupported pcap link type: {0}")]
    UnsupportedLinkType(i32),

    #[error("truncated or malformed link-layer frame: {0}")]
    InvalidLinkFrame(&'static str),

    #[error("truncated or malformed network/transport headers: {0}")]
    InvalidHeaders(&'static str),

    #[error("captured length exceeds u32::MAX")]
    CapturedLengthOverflow,
}

pub fn parse_packet(packet: &OwnedPacket) -> Result<Option<PacketObservation>, PacketParseError> {
    parse_frame(
        packet.linktype,
        &packet.captured,
        packet.wire_len,
        packet.timestamp_micros,
    )
}

pub fn parse_frame(
    linktype: Linktype,
    captured: &[u8],
    wire_len: u32,
    timestamp_micros: i64,
) -> Result<Option<PacketObservation>, PacketParseError> {
    let headers = decode(linktype, captured)?;

    let (source_ip, dest_ip) = match &headers.net {
        Some(NetHeaders::Ipv4(header, _)) => (
            IpAddr::from(header.source),
            IpAddr::from(header.destination),
        ),
        Some(NetHeaders::Ipv6(header, _)) => (
            IpAddr::from(header.source),
            IpAddr::from(header.destination),
        ),
        _ => return Ok(None),
    };

    let (source_port, dest_port, protocol, tcp_metadata) = match &headers.transport {
        Some(TransportHeader::Tcp(tcp)) => {
            let payload = match &headers.payload {
                LaxPayloadSlice::Tcp { payload, .. } => payload.to_vec(),
                _ => Vec::new(),
            };

            let payload = if payload.len() > MAX_TLS_BUFFER {
                payload[..MAX_TLS_BUFFER].to_vec()
            } else {
                payload
            };

            (
                tcp.source_port,
                tcp.destination_port,
                TransportProtocol::Tcp,
                Some(TcpMetadata {
                    sequence: tcp.sequence_number,
                    syn: tcp.syn,
                    ack: tcp.ack,
                    fin: tcp.fin,
                    rst: tcp.rst,
                    payload,
                }),
            )
        }
        Some(TransportHeader::Udp(udp)) => (
            udp.source_port,
            udp.destination_port,
            TransportProtocol::Udp,
            None,
        ),
        _ => return Ok(None),
    };

    if source_port != TLS_PORT && dest_port != TLS_PORT {
        return Ok(None);
    }

    let captured_len_u32 =
        u32::try_from(captured.len()).map_err(|_| PacketParseError::CapturedLengthOverflow)?;
    let wire_len = wire_len.max(captured_len_u32);

    let source = Endpoint {
        ip: source_ip,
        port: source_port,
    };
    let destination = Endpoint {
        ip: dest_ip,
        port: dest_port,
    };
    let flow = FlowKey::canonical(protocol, source.clone(), destination.clone());

    Ok(Some(PacketObservation {
        timestamp_micros,
        flow,
        source,
        destination,
        wire_len: u64::from(wire_len),
        tcp: tcp_metadata,
    }))
}

fn decode(linktype: Linktype, frame: &[u8]) -> Result<LaxPacketHeaders<'_>, PacketParseError> {
    match linktype.0 {
        1 => LaxPacketHeaders::from_ethernet(frame)
            .map_err(|_| PacketParseError::InvalidLinkFrame("ethernet")),
        0 | 108 => {
            let ip = frame
                .get(4..)
                .ok_or(PacketParseError::InvalidLinkFrame("null/loop"))?;
            LaxPacketHeaders::from_ip(ip).map_err(|_| PacketParseError::InvalidHeaders("ip"))
        }
        12 | 101 | 228 | 229 => {
            LaxPacketHeaders::from_ip(frame).map_err(|_| PacketParseError::InvalidHeaders("ip"))
        }
        value => Err(PacketParseError::UnsupportedLinkType(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_tcp_frame(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
        let builder =
            etherparse::PacketBuilder::ethernet2([0, 1, 2, 3, 4, 5], [6, 7, 8, 9, 10, 11])
                .ipv4([10, 0, 0, 2], [93, 184, 216, 34], 64)
                .tcp(source_port, destination_port, 1, 64_240);
        let mut frame = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut frame, payload).unwrap();
        frame
    }

    fn build_udp_frame(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
        let builder =
            etherparse::PacketBuilder::ethernet2([0, 1, 2, 3, 4, 5], [6, 7, 8, 9, 10, 11])
                .ipv4([10, 0, 0, 2], [93, 184, 216, 34], 64)
                .udp(source_port, destination_port);
        let mut frame = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut frame, payload).unwrap();
        frame
    }

    #[test]
    fn parses_ipv4_tcp_client_hello_frame() {
        let frame = build_tcp_frame(50_000, 443, &[0x16, 0x03, 0x03, 0x00, 0x00]);

        let result = parse_frame(Linktype::ETHERNET, &frame, frame.len() as u32, 1000).unwrap();
        let obs = result.expect("TCP/443 packet must be accepted");

        assert_eq!(obs.source.port, 50_000);
        assert_eq!(obs.destination.port, 443);
        assert_eq!(obs.flow.protocol, TransportProtocol::Tcp);
        assert!(obs.tcp.is_some());
        assert_eq!(obs.tcp.unwrap().sequence, 1);
    }

    #[test]
    fn parses_ipv6_tcp_frame() {
        let builder =
            etherparse::PacketBuilder::ethernet2([0, 1, 2, 3, 4, 5], [6, 7, 8, 9, 10, 11])
                .ipv6(
                    [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                    [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
                    64,
                )
                .tcp(50_001, 443, 1, 64_240);

        let payload = vec![0x16, 0x03, 0x01, 0x00, 0x05];
        let mut frame = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut frame, &payload).unwrap();

        let result = parse_frame(Linktype::ETHERNET, &frame, frame.len() as u32, 2000).unwrap();
        let obs = result.expect("IPv6 TCP/443 packet must be accepted");

        assert_eq!(obs.source.port, 50_001);
        assert_eq!(obs.destination.port, 443);
        assert_eq!(obs.flow.protocol, TransportProtocol::Tcp);
        assert!(obs.tcp.is_some());
    }

    #[test]
    fn parses_udp_443_as_unknown_candidate() {
        let frame = build_udp_frame(50_002, 443, &[0x00, 0x01, 0x02, 0x03]);

        let result = parse_frame(Linktype::ETHERNET, &frame, frame.len() as u32, 3000).unwrap();
        let obs = result.expect("UDP/443 packet must be accepted");

        assert_eq!(obs.source.port, 50_002);
        assert_eq!(obs.destination.port, 443);
        assert_eq!(obs.flow.protocol, TransportProtocol::Udp);
        assert!(obs.tcp.is_none());
    }

    #[test]
    fn tcp_and_udp_same_tuple_use_distinct_flow_keys() {
        let tcp_frame = build_tcp_frame(50_002, 443, &[]);
        let udp_frame = build_udp_frame(50_002, 443, &[]);

        let tcp = parse_frame(Linktype::ETHERNET, &tcp_frame, tcp_frame.len() as u32, 1)
            .unwrap()
            .unwrap();
        let udp = parse_frame(Linktype::ETHERNET, &udp_frame, udp_frame.len() as u32, 2)
            .unwrap()
            .unwrap();

        assert_ne!(tcp.flow, udp.flow);
        assert_eq!(tcp.flow.protocol, TransportProtocol::Tcp);
        assert_eq!(udp.flow.protocol, TransportProtocol::Udp);
    }

    #[test]
    fn skips_non_443_transport() {
        let frame = build_tcp_frame(50_003, 80, b"GET ");
        let result = parse_frame(Linktype::ETHERNET, &frame, frame.len() as u32, 4000).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn rejects_unsupported_linktype() {
        let frame = vec![0u8; 100];
        let result = parse_frame(Linktype(999), &frame, 100, 5000);
        assert!(matches!(
            result,
            Err(PacketParseError::UnsupportedLinkType(999))
        ));
    }
}
