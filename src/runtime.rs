use crate::aggregate::DomainAccumulator;
use crate::capture::{CaptureError, CaptureRead, PacketSource};
use crate::flow::{FlowTracker, FlowTrackerConfig};
use crate::model::{DEFAULT_BPF_FILTER, DEFAULT_IDLE_TIMEOUT_SECS};
use crate::packet::parse_packet;
use crate::storage::{StorageError, StorageWriter};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub flush_interval: Duration,
    pub idle_timeout: Duration,
    pub bpf_filter: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            bpf_filter: DEFAULT_BPF_FILTER.to_string(),
        }
    }
}

impl RuntimeConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.flush_interval < Duration::from_millis(100)
            || self.flush_interval > Duration::from_secs(60)
        {
            return Err(RuntimeError::InvalidConfig(
                "flush_interval must be in 100ms..=60s",
            ));
        }
        if self.idle_timeout < Duration::from_secs(1)
            || self.idle_timeout > Duration::from_secs(86400)
        {
            return Err(RuntimeError::InvalidConfig(
                "idle_timeout must be in 1s..=24h",
            ));
        }
        if self.bpf_filter.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("bpf_filter must not be empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub captured_packets: u64,
    pub accepted_packets: u64,
    pub skipped_packets: u64,
    pub parse_errors: u64,
    pub resolved_flows: u64,
    pub evicted_flows: u64,
    pub submitted_batches: u64,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error("invalid runtime configuration: {0}")]
    InvalidConfig(&'static str),

    #[error("failed to install Ctrl+C handler: {0}")]
    CtrlC(#[from] ctrlc::Error),
}

pub fn run_live(
    interface: &str,
    db_path: &Path,
    config: RuntimeConfig,
) -> Result<RunSummary, RuntimeError> {
    config.validate()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        signal_flag.store(true, Ordering::SeqCst);
    })?;

    let live_config = crate::capture::LiveCaptureConfig {
        interface: interface.to_string(),
        bpf_filter: config.bpf_filter.clone(),
        snaplen: crate::model::DEFAULT_SNAPLEN,
        buffer_size: crate::model::DEFAULT_BUFFER_SIZE,
        timeout_ms: crate::model::DEFAULT_TIMEOUT_MS,
    };

    let mut source = crate::capture::open_live(&live_config)?;
    let writer = StorageWriter::spawn(db_path.to_path_buf())?;

    run_source(&mut source, writer, &config, &shutdown, false)
}

pub fn run_offline(
    pcap_path: &Path,
    db_path: &Path,
    config: RuntimeConfig,
) -> Result<RunSummary, RuntimeError> {
    config.validate()?;

    let mut source = crate::capture::open_offline(pcap_path, &config.bpf_filter)?;
    let writer = StorageWriter::spawn(db_path.to_path_buf())?;
    let shutdown = AtomicBool::new(false);

    run_source(&mut source, writer, &config, &shutdown, true)
}

fn run_source<S: PacketSource>(
    source: &mut S,
    writer: StorageWriter,
    config: &RuntimeConfig,
    shutdown: &AtomicBool,
    offline: bool,
) -> Result<RunSummary, RuntimeError> {
    let mut tracker = FlowTracker::new(FlowTrackerConfig {
        idle_timeout_micros: i64::try_from(config.idle_timeout.as_micros())
            .map_err(|_| RuntimeError::InvalidConfig("idle timeout overflow"))?,
        ..FlowTrackerConfig::default()
    });

    let mut accumulator = DomainAccumulator::new();
    let mut last_flush = Instant::now();
    let mut summary = RunSummary::default();

    let loop_result = run_capture_loop(
        source,
        &mut tracker,
        &mut accumulator,
        &mut last_flush,
        &mut summary,
        config,
        shutdown,
        offline,
        &writer,
    );

    finalize_run(
        &mut tracker,
        &mut accumulator,
        writer,
        &mut summary,
        loop_result,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_capture_loop<S: PacketSource>(
    source: &mut S,
    tracker: &mut FlowTracker,
    accumulator: &mut DomainAccumulator,
    last_flush: &mut Instant,
    summary: &mut RunSummary,
    config: &RuntimeConfig,
    shutdown: &AtomicBool,
    offline: bool,
    writer: &StorageWriter,
) -> Result<(), RuntimeError> {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        match source.next_packet() {
            Ok(CaptureRead::Packet(packet)) => {
                summary.captured_packets = summary.captured_packets.saturating_add(1);

                let timestamp_micros = packet.timestamp_micros;

                match parse_packet(&packet) {
                    Ok(Some(observation)) => {
                        summary.accepted_packets = summary.accepted_packets.saturating_add(1);
                        let update = tracker.observe(observation);
                        if update.newly_resolved_domain.is_some() {
                            summary.resolved_flows = summary.resolved_flows.saturating_add(1);
                        }
                        summary.evicted_flows = summary
                            .evicted_flows
                            .saturating_add(update.evicted_flows as u64);
                        for delta in update.deltas {
                            accumulator.add(delta);
                        }
                    }
                    Ok(None) => {
                        summary.skipped_packets = summary.skipped_packets.saturating_add(1);
                    }
                    Err(_) => {
                        summary.parse_errors = summary.parse_errors.saturating_add(1);
                    }
                }

                let expired = tracker.expire_idle(timestamp_micros);
                for delta in expired {
                    accumulator.add(delta);
                }
            }
            Ok(CaptureRead::Timeout) => {
                if !offline {
                    let now_micros = system_now_micros()?;
                    let expired = tracker.expire_idle(now_micros);
                    for delta in expired {
                        accumulator.add(delta);
                    }
                }
            }
            Ok(CaptureRead::EndOfFile) => {
                break;
            }
            Err(e) => {
                return Err(RuntimeError::Capture(e));
            }
        }

        if last_flush.elapsed() >= config.flush_interval {
            let batch = accumulator.drain();
            if !batch.is_empty() {
                writer.submit(batch).map_err(RuntimeError::Storage)?;
                summary.submitted_batches = summary.submitted_batches.saturating_add(1);
            }

            if let Some(err) = writer.poll_error() {
                return Err(RuntimeError::Storage(StorageError::WriterFailed(err)));
            }

            *last_flush = Instant::now();
        }
    }

    Ok(())
}

fn finalize_run(
    tracker: &mut FlowTracker,
    accumulator: &mut DomainAccumulator,
    writer: StorageWriter,
    summary: &mut RunSummary,
    loop_result: Result<(), RuntimeError>,
) -> Result<RunSummary, RuntimeError> {
    let drain_deltas = tracker.drain_all();
    for delta in drain_deltas {
        accumulator.add(delta);
    }

    let batch = accumulator.drain();
    let mut submit_err = None;
    if !batch.is_empty() {
        match writer.submit(batch) {
            Ok(()) => {
                summary.submitted_batches = summary.submitted_batches.saturating_add(1);
            }
            Err(e) => {
                submit_err = Some(e);
            }
        }
    }

    let shutdown_result = writer.shutdown();

    if let Some(e) = submit_err {
        return Err(RuntimeError::Storage(e));
    }

    if let Err(e) = shutdown_result {
        return Err(RuntimeError::Storage(e));
    }

    loop_result?;
    Ok(summary.clone())
}

fn system_now_micros() -> Result<i64, RuntimeError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::InvalidConfig("system clock before Unix epoch"))?;
    Ok(duration.as_micros() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureError, CaptureRead, OwnedPacket, PacketSource};
    use pcap::Linktype;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    struct FakePacketSource {
        events: VecDeque<Result<CaptureRead, CaptureError>>,
        lt: Linktype,
    }

    impl FakePacketSource {
        fn new(events: VecDeque<Result<CaptureRead, CaptureError>>) -> Self {
            Self {
                events,
                lt: Linktype::ETHERNET,
            }
        }
    }

    impl PacketSource for FakePacketSource {
        fn linktype(&self) -> Linktype {
            self.lt
        }

        fn next_packet(&mut self) -> Result<CaptureRead, CaptureError> {
            self.events
                .pop_front()
                .unwrap_or(Ok(CaptureRead::EndOfFile))
        }
    }

    fn tcp_syn_packet(ts_sec: i64, ts_usec: i64, src_ip: [u8; 4], dst_ip: [u8; 4]) -> OwnedPacket {
        let mut data = vec![0u8; 66];
        data[0..6].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data[6..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data[12..14].copy_from_slice(&[0x08, 0x00]);
        data[14] = 0x45;
        data[14 + 9] = 0x06;
        data[14 + 12..14 + 16].copy_from_slice(&src_ip);
        data[14 + 16..14 + 20].copy_from_slice(&dst_ip);
        data[34] = 0x00;
        data[35] = 0x2B;
        data[36] = 0x00;
        data[37] = 0x00;
        data[38] = 0x50;
        data[39] = 0x00;
        data[39] |= 0x02;
        data[40..42].copy_from_slice(&0u16.to_be_bytes());
        data[42..46].copy_from_slice(&1u32.to_be_bytes());
        data[46..50].copy_from_slice(&0u32.to_be_bytes());

        OwnedPacket {
            timestamp_micros: ts_sec * 1_000_000 + ts_usec,
            wire_len: data.len() as u32,
            captured: data.into(),
            linktype: Linktype::ETHERNET,
        }
    }

    fn empty_tcp_packet(
        ts_sec: i64,
        ts_usec: i64,
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
    ) -> OwnedPacket {
        let mut data = vec![0u8; 66];
        data[0..6].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data[6..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data[12..14].copy_from_slice(&[0x08, 0x00]);
        data[14] = 0x45;
        data[14 + 9] = 0x06;
        data[14 + 12..14 + 16].copy_from_slice(&src_ip);
        data[14 + 16..14 + 20].copy_from_slice(&dst_ip);
        data[34] = 0x00;
        data[35] = 0x2B;
        data[36] = 0x00;
        data[37] = 0x00;
        data[38] = 0x50;
        data[39] = 0x00;
        data[40..42].copy_from_slice(&0u16.to_be_bytes());
        data[42..46].copy_from_slice(&1u32.to_be_bytes());
        data[46..50].copy_from_slice(&0u32.to_be_bytes());

        OwnedPacket {
            timestamp_micros: ts_sec * 1_000_000 + ts_usec,
            wire_len: data.len() as u32,
            captured: data.into(),
            linktype: Linktype::ETHERNET,
        }
    }

    fn make_runtime_config() -> RuntimeConfig {
        RuntimeConfig {
            flush_interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(300),
            bpf_filter: DEFAULT_BPF_FILTER.to_string(),
        }
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let id = format!(
            "runtime_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            name
        );
        path.push(id);
        path.set_extension("db");
        path
    }

    #[test]
    fn default_runtime_config_is_fixed() {
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.flush_interval, Duration::from_secs(1));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(300));
        assert_eq!(cfg.bpf_filter, DEFAULT_BPF_FILTER);
    }

    #[test]
    fn invalid_runtime_config_is_rejected() {
        let cfg = RuntimeConfig {
            flush_interval: Duration::from_millis(99),
            idle_timeout: Duration::from_secs(300),
            bpf_filter: DEFAULT_BPF_FILTER.to_string(),
        };
        assert!(matches!(
            cfg.validate(),
            Err(RuntimeError::InvalidConfig(_))
        ));

        let cfg2 = RuntimeConfig {
            flush_interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(300),
            bpf_filter: "   ".to_string(),
        };
        assert!(matches!(
            cfg2.validate(),
            Err(RuntimeError::InvalidConfig(_))
        ));
    }

    #[test]
    fn fake_source_processes_packet_and_eof() {
        let mut events: VecDeque<Result<CaptureRead, CaptureError>> = VecDeque::new();

        let packet = empty_tcp_packet(1_700_000_000, 0, [192, 168, 1, 1], [93, 184, 216, 34]);
        events.push_back(Ok(CaptureRead::Packet(packet)));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let config = make_runtime_config();
        let shutdown = AtomicBool::new(false);
        let db_path = temp_db_path("pkt_eof");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();

        let summary = run_source(&mut source, writer, &config, &shutdown, true).unwrap();

        assert_eq!(summary.captured_packets, 1);
        assert!(summary.accepted_packets >= 1 || summary.skipped_packets >= 1);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    #[test]
    fn fake_source_final_flushes_unresolved_flow() {
        let mut events: VecDeque<Result<CaptureRead, CaptureError>> = VecDeque::new();

        let syn = tcp_syn_packet(1_700_000_000, 0, [192, 168, 1, 1], [93, 184, 216, 34]);
        events.push_back(Ok(CaptureRead::Packet(syn)));

        let mut p2_data = vec![0u8; 66];
        p2_data[0..6].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        p2_data[6..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        p2_data[12..14].copy_from_slice(&[0x08, 0x00]);
        p2_data[14] = 0x45;
        p2_data[14 + 9] = 0x06;
        p2_data[14 + 12..14 + 16].copy_from_slice(&[93, 184, 216, 34]);
        p2_data[14 + 16..14 + 20].copy_from_slice(&[192, 168, 1, 1]);
        p2_data[34] = 0x00;
        p2_data[35] = 0x2B;
        p2_data[36] = 0x00;
        p2_data[37] = 0x00;
        p2_data[38] = 0x50;
        p2_data[39] = 0x10;
        let ack_data = OwnedPacket {
            timestamp_micros: 1_700_000_000_100_000,
            wire_len: p2_data.len() as u32,
            captured: p2_data.into(),
            linktype: Linktype::ETHERNET,
        };
        events.push_back(Ok(CaptureRead::Packet(ack_data)));

        let fin_data = {
            let mut d = vec![0u8; 66];
            d[0..6].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
            d[6..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
            d[12..14].copy_from_slice(&[0x08, 0x00]);
            d[14] = 0x45;
            d[14 + 9] = 0x06;
            d[14 + 12..14 + 16].copy_from_slice(&[192, 168, 1, 1]);
            d[14 + 16..14 + 20].copy_from_slice(&[93, 184, 216, 34]);
            d[34] = 0x00;
            d[35] = 0x2B;
            d[36] = 0x00;
            d[37] = 0x00;
            d[38] = 0x50;
            d[39] = 0x11;
            OwnedPacket {
                timestamp_micros: 1_700_000_000_200_000,
                wire_len: d.len() as u32,
                captured: d.into(),
                linktype: Linktype::ETHERNET,
            }
        };
        events.push_back(Ok(CaptureRead::Packet(fin_data)));

        let ack2_data = {
            let mut d = vec![0u8; 66];
            d[0..6].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
            d[6..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
            d[12..14].copy_from_slice(&[0x08, 0x00]);
            d[14] = 0x45;
            d[14 + 9] = 0x06;
            d[14 + 12..14 + 16].copy_from_slice(&[93, 184, 216, 34]);
            d[14 + 16..14 + 20].copy_from_slice(&[192, 168, 1, 1]);
            d[34] = 0x00;
            d[35] = 0x2B;
            d[36] = 0x00;
            d[37] = 0x00;
            d[38] = 0x50;
            d[39] = 0x10;
            OwnedPacket {
                timestamp_micros: 1_700_000_000_300_000,
                wire_len: d.len() as u32,
                captured: d.into(),
                linktype: Linktype::ETHERNET,
            }
        };
        events.push_back(Ok(CaptureRead::Packet(ack2_data)));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let config = make_runtime_config();
        let shutdown = AtomicBool::new(false);
        let db_path = temp_db_path("unresolved");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();

        let summary = run_source(&mut source, writer, &config, &shutdown, true).unwrap();

        assert_eq!(summary.captured_packets, 4);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    #[test]
    fn parse_error_is_counted_and_loop_continues() {
        let mut events: VecDeque<Result<CaptureRead, CaptureError>> = VecDeque::new();

        let bad_packet = OwnedPacket {
            timestamp_micros: 1_700_000_000,
            wire_len: 3,
            captured: vec![0xFF, 0xFF, 0xFF].into(),
            linktype: Linktype::ETHERNET,
        };
        events.push_back(Ok(CaptureRead::Packet(bad_packet)));

        let packet = empty_tcp_packet(1_700_000_001, 0, [192, 168, 1, 1], [93, 184, 216, 34]);
        events.push_back(Ok(CaptureRead::Packet(packet)));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let config = make_runtime_config();
        let shutdown = AtomicBool::new(false);
        let db_path = temp_db_path("parse_err");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();

        let summary = run_source(&mut source, writer, &config, &shutdown, true).unwrap();

        assert_eq!(summary.captured_packets, 2);
        assert!(summary.parse_errors >= 1);
        assert_eq!(summary.captured_packets - summary.parse_errors, 1);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    #[test]
    fn timeout_does_not_count_as_packet() {
        let mut events: VecDeque<Result<CaptureRead, CaptureError>> = VecDeque::new();
        events.push_back(Ok(CaptureRead::Timeout));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let config = make_runtime_config();
        let shutdown = AtomicBool::new(false);
        let db_path = temp_db_path("timeout");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();

        let summary = run_source(&mut source, writer, &config, &shutdown, true).unwrap();

        assert_eq!(summary.captured_packets, 0);
        assert_eq!(summary.accepted_packets, 0);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }
}
