use pcap::{Active, Capture, Device, Linktype, Offline};
use std::path::Path;
use thiserror::Error;

use crate::model::{DEFAULT_BPF_FILTER, DEFAULT_BUFFER_SIZE, DEFAULT_SNAPLEN, DEFAULT_TIMEOUT_MS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDeviceInfo {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCaptureConfig {
    pub interface: String,
    pub bpf_filter: String,
    pub snaplen: i32,
    pub buffer_size: i32,
    pub timeout_ms: i32,
}

impl LiveCaptureConfig {
    pub fn for_interface(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            bpf_filter: DEFAULT_BPF_FILTER.to_string(),
            snaplen: DEFAULT_SNAPLEN,
            buffer_size: DEFAULT_BUFFER_SIZE,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    pub fn validate(&self) -> Result<(), CaptureError> {
        if self.interface.trim().is_empty() {
            return Err(CaptureError::InvalidConfig("interface must not be empty"));
        }
        if self.bpf_filter.trim().is_empty() {
            return Err(CaptureError::InvalidConfig("BPF filter must not be empty"));
        }
        if self.snaplen < 256 {
            return Err(CaptureError::InvalidConfig("snaplen must be at least 256"));
        }
        if self.buffer_size < 1_000_000 {
            return Err(CaptureError::InvalidConfig(
                "buffer_size must be at least 1000000",
            ));
        }
        if !(1..=5_000).contains(&self.timeout_ms) {
            return Err(CaptureError::InvalidConfig(
                "timeout_ms must be in 1..=5000",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedPacket {
    pub timestamp_micros: i64,
    pub wire_len: u32,
    pub captured: Box<[u8]>,
    pub linktype: Linktype,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRead {
    Packet(OwnedPacket),
    Timeout,
    EndOfFile,
}

pub trait PacketSource {
    fn linktype(&self) -> Linktype;
    fn next_packet(&mut self) -> Result<CaptureRead, CaptureError>;
}

pub struct LivePacketSource {
    cap: Capture<Active>,
    linktype: Linktype,
}

pub struct OfflinePacketSource {
    cap: Capture<Offline>,
    linktype: Linktype,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("pcap error: {0}")]
    Pcap(#[from] pcap::Error),

    #[error("capture interface not found: {requested}; available={available:?}")]
    InterfaceNotFound {
        requested: String,
        available: Vec<String>,
    },

    #[error("invalid capture configuration: {0}")]
    InvalidConfig(&'static str),

    #[error("pcap timestamp microseconds out of range: {0}")]
    TimestampFractionOutOfRange(i64),

    #[error("pcap timestamp cannot be represented as microseconds")]
    TimestampOverflow,
}

pub fn list_devices() -> Result<Vec<CaptureDeviceInfo>, CaptureError> {
    let devices = Device::list()?;
    let mut result: Vec<CaptureDeviceInfo> = devices
        .into_iter()
        .map(|d| CaptureDeviceInfo {
            name: d.name,
            description: d.desc,
        })
        .collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

pub fn open_live(config: &LiveCaptureConfig) -> Result<LivePacketSource, CaptureError> {
    config.validate()?;

    let devices = Device::list()?;
    let device_name = devices
        .iter()
        .find(|d| d.name == config.interface)
        .map(|d| d.name.as_str());

    if device_name.is_none() {
        let available: Vec<String> = devices.into_iter().map(|d| d.name).collect();
        return Err(CaptureError::InterfaceNotFound {
            requested: config.interface.clone(),
            available,
        });
    }

    let mut capture = Capture::from_device(device_name.unwrap())?
        .promisc(false)
        .snaplen(config.snaplen)
        .buffer_size(config.buffer_size)
        .timeout(config.timeout_ms)
        .immediate_mode(true)
        .open()?;

    capture.filter(&config.bpf_filter, true)?;

    let linktype = capture.get_datalink();

    Ok(LivePacketSource {
        cap: capture,
        linktype,
    })
}

pub fn open_offline(path: &Path, bpf_filter: &str) -> Result<OfflinePacketSource, CaptureError> {
    if bpf_filter.trim().is_empty() {
        return Err(CaptureError::InvalidConfig("BPF filter must not be empty"));
    }

    let mut capture = Capture::from_file(path)?;
    capture.filter(bpf_filter, true)?;

    let linktype = capture.get_datalink();

    Ok(OfflinePacketSource {
        cap: capture,
        linktype,
    })
}

impl PacketSource for LivePacketSource {
    fn linktype(&self) -> Linktype {
        self.linktype
    }

    fn next_packet(&mut self) -> Result<CaptureRead, CaptureError> {
        match self.cap.next_packet() {
            Ok(packet) => {
                let timestamp_micros = timestamp_to_micros(
                    packet.header.ts.tv_sec.into(),
                    packet.header.ts.tv_usec.into(),
                )?;
                let wire_len = packet.header.len;
                let captured: Box<[u8]> = packet.data.into();

                Ok(CaptureRead::Packet(OwnedPacket {
                    timestamp_micros,
                    wire_len,
                    captured,
                    linktype: self.linktype,
                }))
            }
            Err(pcap::Error::TimeoutExpired) => Ok(CaptureRead::Timeout),
            Err(pcap::Error::NoMorePackets) => Ok(CaptureRead::EndOfFile),
            Err(e) => Err(CaptureError::Pcap(e)),
        }
    }
}

impl PacketSource for OfflinePacketSource {
    fn linktype(&self) -> Linktype {
        self.linktype
    }

    fn next_packet(&mut self) -> Result<CaptureRead, CaptureError> {
        match self.cap.next_packet() {
            Ok(packet) => {
                let timestamp_micros = timestamp_to_micros(
                    packet.header.ts.tv_sec.into(),
                    packet.header.ts.tv_usec.into(),
                )?;
                let wire_len = packet.header.len;
                let captured: Box<[u8]> = packet.data.into();

                Ok(CaptureRead::Packet(OwnedPacket {
                    timestamp_micros,
                    wire_len,
                    captured,
                    linktype: self.linktype,
                }))
            }
            Err(pcap::Error::TimeoutExpired) => Ok(CaptureRead::Timeout),
            Err(pcap::Error::NoMorePackets) => Ok(CaptureRead::EndOfFile),
            Err(e) => Err(CaptureError::Pcap(e)),
        }
    }
}

fn timestamp_to_micros(tv_sec: i64, tv_usec: i64) -> Result<i64, CaptureError> {
    if !(0..=999_999).contains(&tv_usec) {
        return Err(CaptureError::TimestampFractionOutOfRange(tv_usec));
    }

    let sec_micros = tv_sec
        .checked_mul(1_000_000)
        .ok_or(CaptureError::TimestampOverflow)?;

    sec_micros
        .checked_add(tv_usec)
        .ok_or(CaptureError::TimestampOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_live_config_is_fixed() {
        let config = LiveCaptureConfig::for_interface("test");
        assert_eq!(config.snaplen, 65535);
        assert_eq!(config.buffer_size, 4_000_000);
        assert_eq!(config.timeout_ms, 250);
        assert_eq!(config.bpf_filter, DEFAULT_BPF_FILTER);
    }

    #[test]
    fn timestamp_conversion_is_microseconds() {
        let result = timestamp_to_micros(1_700_000_000, 123_456).unwrap();
        assert_eq!(result, 1_700_000_000_123_456);
    }

    #[test]
    fn timestamp_fraction_out_of_range_is_rejected() {
        let result = timestamp_to_micros(1, 1_000_000);
        assert!(matches!(
            result,
            Err(CaptureError::TimestampFractionOutOfRange(1_000_000))
        ));
    }

    #[test]
    fn timestamp_overflow_is_rejected() {
        let result = timestamp_to_micros(i64::MAX, 0);
        assert!(matches!(result, Err(CaptureError::TimestampOverflow)));
    }
}
