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
            || self.idle_timeout > Duration::from_secs(86_400)
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

    run_live_with_shutdown(interface, db_path, config, shutdown)
}

pub fn run_live_with_shutdown(
    interface: &str,
    db_path: &Path,
    config: RuntimeConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<RunSummary, RuntimeError> {
    config.validate()?;

    let live_config = crate::capture::LiveCaptureConfig {
        interface: interface.to_string(),
        bpf_filter: config.bpf_filter.clone(),
        snaplen: crate::model::DEFAULT_SNAPLEN,
        buffer_size: crate::model::DEFAULT_BUFFER_SIZE,
        timeout_ms: crate::model::DEFAULT_TIMEOUT_MS,
    };

    let mut source = crate::capture::open_live(&live_config)?;
    let writer = StorageWriter::spawn(db_path.to_path_buf())?;

    run_source(&mut source, writer, &config, shutdown.as_ref(), false)
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

    let finalization_result = finalize_run(&mut tracker, &mut accumulator, writer, &mut summary);

    complete_run(loop_result, finalization_result)?;
    Ok(summary)
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
                        accumulator.add_all(update.deltas);
                    }
                    Ok(None) => {
                        summary.skipped_packets = summary.skipped_packets.saturating_add(1);
                    }
                    Err(_) => {
                        summary.parse_errors = summary.parse_errors.saturating_add(1);
                    }
                }

                accumulator.add_all(tracker.expire_idle(timestamp_micros));
            }
            Ok(CaptureRead::Timeout) => {
                if !offline {
                    accumulator.add_all(tracker.expire_idle(system_now_micros()?));
                }
            }
            Ok(CaptureRead::EndOfFile) => break,
            Err(error) => return Err(RuntimeError::Capture(error)),
        }

        if last_flush.elapsed() >= config.flush_interval {
            submit_accumulator(accumulator, writer, summary)?;
            if let Some(error) = writer.poll_error() {
                return Err(RuntimeError::Storage(StorageError::WriterFailed(error)));
            }
            *last_flush = Instant::now();
        }
    }

    Ok(())
}

fn submit_accumulator(
    accumulator: &mut DomainAccumulator,
    writer: &StorageWriter,
    summary: &mut RunSummary,
) -> Result<(), RuntimeError> {
    let batch = accumulator.drain();
    if batch.is_empty() {
        return Ok(());
    }

    writer.submit(batch)?;
    summary.submitted_batches = summary.submitted_batches.saturating_add(1);
    Ok(())
}

fn finalize_run(
    tracker: &mut FlowTracker,
    accumulator: &mut DomainAccumulator,
    writer: StorageWriter,
    summary: &mut RunSummary,
) -> Result<(), RuntimeError> {
    accumulator.add_all(tracker.drain_all());

    let batch = accumulator.drain();
    let submit_result = if batch.is_empty() {
        Ok(())
    } else {
        writer.submit(batch).map(|()| {
            summary.submitted_batches = summary.submitted_batches.saturating_add(1);
        })
    };

    let shutdown_result = writer.shutdown();

    submit_result.map_err(RuntimeError::Storage)?;
    shutdown_result.map_err(RuntimeError::Storage)?;
    Ok(())
}

fn complete_run(
    loop_result: Result<(), RuntimeError>,
    finalization_result: Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    loop_result.and(finalization_result)
}

fn system_now_micros() -> Result<i64, RuntimeError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::InvalidConfig("system clock before Unix epoch"))?;
    i64::try_from(duration.as_micros())
        .map_err(|_| RuntimeError::InvalidConfig("system time microseconds overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureRead, OwnedPacket};
    use crate::model::UNKNOWN_DOMAIN;
    use crate::storage::Storage;
    use pcap::Linktype;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    struct FakePacketSource {
        events: VecDeque<Result<CaptureRead, CaptureError>>,
        linktype: Linktype,
    }

    impl FakePacketSource {
        fn new(events: VecDeque<Result<CaptureRead, CaptureError>>) -> Self {
            Self {
                events,
                linktype: Linktype::ETHERNET,
            }
        }
    }

    impl PacketSource for FakePacketSource {
        fn linktype(&self) -> Linktype {
            self.linktype
        }

        fn next_packet(&mut self) -> Result<CaptureRead, CaptureError> {
            self.events
                .pop_front()
                .unwrap_or(Ok(CaptureRead::EndOfFile))
        }
    }

    fn owned_ethernet_packet(timestamp_micros: i64, data: Vec<u8>) -> OwnedPacket {
        OwnedPacket {
            timestamp_micros,
            wire_len: u32::try_from(data.len()).unwrap(),
            captured: data.into(),
            linktype: Linktype::ETHERNET,
        }
    }

    fn udp_443_packet(timestamp_micros: i64) -> OwnedPacket {
        let payload = b"quic-payload";
        let builder =
            etherparse::PacketBuilder::ethernet2([0, 1, 2, 3, 4, 5], [6, 7, 8, 9, 10, 11])
                .ipv4([10, 0, 0, 1], [93, 184, 216, 34], 64)
                .udp(50_000, 443);
        let mut frame = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut frame, payload).unwrap();
        owned_ethernet_packet(timestamp_micros, frame)
    }

    fn tcp_syn_packet(timestamp_micros: i64) -> OwnedPacket {
        let builder =
            etherparse::PacketBuilder::ethernet2([0, 1, 2, 3, 4, 5], [6, 7, 8, 9, 10, 11])
                .ipv4([10, 0, 0, 1], [93, 184, 216, 34], 64)
                .tcp(50_001, 443, 1, 64_240)
                .syn();
        let mut frame = Vec::with_capacity(builder.size(0));
        builder.write(&mut frame, &[]).unwrap();
        owned_ethernet_packet(timestamp_micros, frame)
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let id = format!(
            "runtime_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            name
        );
        path.push(id);
        path.set_extension("db");
        path
    }

    fn cleanup_db(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn default_runtime_config_is_fixed() {
        let config = RuntimeConfig::default();
        assert_eq!(config.flush_interval, Duration::from_secs(1));
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.bpf_filter, DEFAULT_BPF_FILTER);
    }

    #[test]
    fn invalid_runtime_config_is_rejected() {
        let too_fast = RuntimeConfig {
            flush_interval: Duration::from_millis(99),
            ..RuntimeConfig::default()
        };
        assert!(matches!(
            too_fast.validate(),
            Err(RuntimeError::InvalidConfig(_))
        ));

        let empty_filter = RuntimeConfig {
            bpf_filter: "   ".to_string(),
            ..RuntimeConfig::default()
        };
        assert!(matches!(
            empty_filter.validate(),
            Err(RuntimeError::InvalidConfig(_))
        ));
    }

    #[test]
    fn fake_source_processes_udp_and_persists_unknown() {
        let mut events = VecDeque::new();
        events.push_back(Ok(CaptureRead::Packet(udp_443_packet(
            1_700_000_000_000_000,
        ))));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let db_path = temp_db_path("udp_unknown");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);

        let summary = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            true,
        )
        .unwrap();

        assert_eq!(summary.captured_packets, 1);
        assert_eq!(summary.accepted_packets, 1);
        assert_eq!(summary.parse_errors, 0);

        let storage = Storage::open(&db_path).unwrap();
        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(rows[0].bytes, 54);
        assert_eq!(rows[0].packets, 1);

        drop(storage);
        cleanup_db(&db_path);
    }

    #[test]
    fn final_flush_persists_unresolved_tcp_flow() {
        let mut events = VecDeque::new();
        events.push_back(Ok(CaptureRead::Packet(tcp_syn_packet(
            1_700_000_000_000_000,
        ))));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let db_path = temp_db_path("tcp_unknown");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);

        let summary = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            true,
        )
        .unwrap();
        assert_eq!(summary.captured_packets, 1);
        assert_eq!(summary.accepted_packets, 1);

        let storage = Storage::open(&db_path).unwrap();
        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].domain, UNKNOWN_DOMAIN);
        assert_eq!(rows[0].packets, 1);

        drop(storage);
        cleanup_db(&db_path);
    }

    #[test]
    fn parse_error_is_counted_and_loop_continues() {
        let bad_packet = OwnedPacket {
            timestamp_micros: 1_700_000_000_000_000,
            wire_len: 3,
            captured: vec![0xff, 0xff, 0xff].into(),
            linktype: Linktype::ETHERNET,
        };

        let mut events = VecDeque::new();
        events.push_back(Ok(CaptureRead::Packet(bad_packet)));
        events.push_back(Ok(CaptureRead::Packet(udp_443_packet(
            1_700_000_001_000_000,
        ))));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let db_path = temp_db_path("parse_error");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);

        let summary = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            true,
        )
        .unwrap();

        assert_eq!(summary.captured_packets, 2);
        assert_eq!(summary.parse_errors, 1);
        assert_eq!(summary.accepted_packets, 1);
        cleanup_db(&db_path);
    }

    #[test]
    fn timeout_does_not_count_as_packet() {
        let mut events = VecDeque::new();
        events.push_back(Ok(CaptureRead::Timeout));
        events.push_back(Ok(CaptureRead::EndOfFile));

        let mut source = FakePacketSource::new(events);
        let db_path = temp_db_path("timeout");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);

        let summary = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            true,
        )
        .unwrap();

        assert_eq!(summary.captured_packets, 0);
        assert_eq!(summary.accepted_packets, 0);
        cleanup_db(&db_path);
    }

    #[test]
    fn capture_loop_error_has_priority_over_finalization_error() {
        let loop_error = Err(RuntimeError::Capture(CaptureError::InvalidConfig(
            "loop failed",
        )));
        let finalization_error = Err(RuntimeError::Storage(StorageError::WriterPanicked));

        let result = complete_run(loop_error, finalization_error);
        assert!(matches!(
            result,
            Err(RuntimeError::Capture(CaptureError::InvalidConfig(
                "loop failed"
            )))
        ));
    }

    #[test]
    fn source_error_is_returned_after_best_effort_finalization() {
        let mut events = VecDeque::new();
        events.push_back(Err(CaptureError::InvalidConfig("capture failed")));

        let mut source = FakePacketSource::new(events);
        let db_path = temp_db_path("source_error");
        let writer = StorageWriter::spawn(db_path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);

        let result = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            true,
        );

        assert!(matches!(
            result,
            Err(RuntimeError::Capture(CaptureError::InvalidConfig(
                "capture failed"
            )))
        ));
        cleanup_db(&db_path);
    }
}
