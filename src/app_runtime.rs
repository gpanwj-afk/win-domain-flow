use crate::aggregate::{ApplicationAccumulator, DomainAccumulator};
use crate::app_storage::{ApplicationStorage, ApplicationStorageError};
use crate::app_tracker::ApplicationTracker;
use crate::attribution::{create_process_attributor, ProcessAttributor};
use crate::capture::{CaptureError, CaptureRead, PacketSource};
use crate::flow::{FlowTracker, FlowTrackerConfig};
use crate::model::{
    ApplicationFlushBatch, FlushBatch, UNKNOWN_APPLICATION, DEFAULT_BUFFER_SIZE, DEFAULT_SNAPLEN,
    DEFAULT_TIMEOUT_MS,
};
use crate::packet::parse_packet;
use crate::runtime::RuntimeConfig;
use crate::storage::{Storage, StorageError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationRunSummary {
    pub captured_packets: u64,
    pub accepted_packets: u64,
    pub skipped_packets: u64,
    pub parse_errors: u64,
    pub resolved_flows: u64,
    pub evicted_flows: u64,
    pub submitted_batches: u64,
    pub attributed_packets: u64,
    pub attribution_misses: u64,
    pub attribution_errors: u64,
}

#[derive(Debug, Error)]
pub enum ApplicationRuntimeError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error(transparent)]
    ApplicationStorage(#[from] ApplicationStorageError),

    #[error("invalid runtime configuration: {0}")]
    InvalidConfig(String),

    #[error("product database writer failed: {0}")]
    WriterFailed(String),

    #[error("product database writer channel closed")]
    WriterChannelClosed,

    #[error("product database writer thread panicked")]
    WriterPanicked,
}

pub fn run_live_with_shutdown(
    interface: &str,
    database_path: &Path,
    config: RuntimeConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<ApplicationRunSummary, ApplicationRuntimeError> {
    config
        .validate()
        .map_err(|error| ApplicationRuntimeError::InvalidConfig(error.to_string()))?;

    let live_config = crate::capture::LiveCaptureConfig {
        interface: interface.to_string(),
        bpf_filter: config.bpf_filter.clone(),
        snaplen: DEFAULT_SNAPLEN,
        buffer_size: DEFAULT_BUFFER_SIZE,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    };
    let mut source = crate::capture::open_live(&live_config)?;
    let writer = ProductStorageWriter::spawn(database_path.to_path_buf())?;
    let mut attributor = create_process_attributor();

    run_source(
        &mut source,
        writer,
        &config,
        shutdown.as_ref(),
        attributor.as_mut(),
    )
}

fn run_source<S: PacketSource>(
    source: &mut S,
    writer: ProductStorageWriter,
    config: &RuntimeConfig,
    shutdown: &AtomicBool,
    attributor: &mut dyn ProcessAttributor,
) -> Result<ApplicationRunSummary, ApplicationRuntimeError> {
    let idle_timeout_micros = i64::try_from(config.idle_timeout.as_micros())
        .map_err(|_| ApplicationRuntimeError::InvalidConfig("idle timeout overflow".to_string()))?;
    let mut flow_tracker = FlowTracker::new(FlowTrackerConfig {
        idle_timeout_micros,
        ..FlowTrackerConfig::default()
    });
    let mut application_tracker = ApplicationTracker::new(idle_timeout_micros);
    let mut domain_accumulator = DomainAccumulator::new();
    let mut application_accumulator = ApplicationAccumulator::new();
    let mut last_flush = Instant::now();
    let mut summary = ApplicationRunSummary::default();

    let loop_result = run_capture_loop(
        source,
        &mut flow_tracker,
        &mut application_tracker,
        &mut domain_accumulator,
        &mut application_accumulator,
        &mut last_flush,
        &mut summary,
        config,
        shutdown,
        attributor,
        &writer,
    );

    let finalization_result = finalize_run(
        &mut flow_tracker,
        &mut application_tracker,
        &mut domain_accumulator,
        &mut application_accumulator,
        writer,
        &mut summary,
    );

    loop_result.and(finalization_result)?;
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn run_capture_loop<S: PacketSource>(
    source: &mut S,
    flow_tracker: &mut FlowTracker,
    application_tracker: &mut ApplicationTracker,
    domain_accumulator: &mut DomainAccumulator,
    application_accumulator: &mut ApplicationAccumulator,
    last_flush: &mut Instant,
    summary: &mut ApplicationRunSummary,
    config: &RuntimeConfig,
    shutdown: &AtomicBool,
    attributor: &mut dyn ProcessAttributor,
    writer: &ProductStorageWriter,
) -> Result<(), ApplicationRuntimeError> {
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
                        let application = lookup_application(attributor, &observation.flow, summary);
                        let flow_update = flow_tracker.observe(observation.clone());
                        let application_deltas = application_tracker.observe(
                            &observation,
                            flow_update.newly_resolved_domain.as_deref(),
                            &application,
                        );

                        if flow_update.newly_resolved_domain.is_some() {
                            summary.resolved_flows = summary.resolved_flows.saturating_add(1);
                        }
                        summary.evicted_flows = summary
                            .evicted_flows
                            .saturating_add(flow_update.evicted_flows as u64);
                        domain_accumulator.add_all(flow_update.deltas);
                        application_accumulator.add_all(application_deltas);
                    }
                    Ok(None) => {
                        summary.skipped_packets = summary.skipped_packets.saturating_add(1);
                    }
                    Err(_) => {
                        summary.parse_errors = summary.parse_errors.saturating_add(1);
                    }
                }

                domain_accumulator.add_all(flow_tracker.expire_idle(timestamp_micros));
                application_accumulator.add_all(application_tracker.expire_idle(timestamp_micros));
            }
            Ok(CaptureRead::Timeout) => {
                let now_micros = system_now_micros()?;
                domain_accumulator.add_all(flow_tracker.expire_idle(now_micros));
                application_accumulator.add_all(application_tracker.expire_idle(now_micros));
            }
            Ok(CaptureRead::EndOfFile) => break,
            Err(error) => return Err(ApplicationRuntimeError::Capture(error)),
        }

        if last_flush.elapsed() >= config.flush_interval {
            submit_accumulators(
                domain_accumulator,
                application_accumulator,
                writer,
                summary,
            )?;
            if let Some(error) = writer.poll_error() {
                return Err(ApplicationRuntimeError::WriterFailed(error));
            }
            *last_flush = Instant::now();
        }
    }
    Ok(())
}

fn lookup_application(
    attributor: &mut dyn ProcessAttributor,
    flow: &crate::model::FlowKey,
    summary: &mut ApplicationRunSummary,
) -> String {
    match attributor.lookup(flow) {
        Ok(Some(identity)) => {
            summary.attributed_packets = summary.attributed_packets.saturating_add(1);
            identity.name
        }
        Ok(None) => {
            summary.attribution_misses = summary.attribution_misses.saturating_add(1);
            UNKNOWN_APPLICATION.to_string()
        }
        Err(_) => {
            summary.attribution_errors = summary.attribution_errors.saturating_add(1);
            summary.attribution_misses = summary.attribution_misses.saturating_add(1);
            UNKNOWN_APPLICATION.to_string()
        }
    }
}

fn submit_accumulators(
    domain_accumulator: &mut DomainAccumulator,
    application_accumulator: &mut ApplicationAccumulator,
    writer: &ProductStorageWriter,
    summary: &mut ApplicationRunSummary,
) -> Result<(), ApplicationRuntimeError> {
    let domain_batch = domain_accumulator.drain();
    let application_batch = application_accumulator.drain();
    if domain_batch.is_empty() && application_batch.is_empty() {
        return Ok(());
    }
    writer.submit(domain_batch, application_batch)?;
    summary.submitted_batches = summary.submitted_batches.saturating_add(1);
    Ok(())
}

fn finalize_run(
    flow_tracker: &mut FlowTracker,
    application_tracker: &mut ApplicationTracker,
    domain_accumulator: &mut DomainAccumulator,
    application_accumulator: &mut ApplicationAccumulator,
    writer: ProductStorageWriter,
    summary: &mut ApplicationRunSummary,
) -> Result<(), ApplicationRuntimeError> {
    domain_accumulator.add_all(flow_tracker.drain_all());
    application_accumulator.add_all(application_tracker.drain_all());
    let domain_batch = domain_accumulator.drain();
    let application_batch = application_accumulator.drain();
    let submitted = !domain_batch.is_empty() || !application_batch.is_empty();
    if submitted {
        writer.submit(domain_batch, application_batch)?;
        summary.submitted_batches = summary.submitted_batches.saturating_add(1);
    }
    writer.shutdown()
}

fn system_now_micros() -> Result<i64, ApplicationRuntimeError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApplicationRuntimeError::InvalidConfig("system clock before Unix epoch".to_string()))?;
    i64::try_from(duration.as_micros())
        .map_err(|_| ApplicationRuntimeError::InvalidConfig("system time overflow".to_string()))
}

enum WriterCommand {
    Write(FlushBatch, ApplicationFlushBatch),
    Shutdown,
}

struct ProductStorageWriter {
    tx: SyncSender<WriterCommand>,
    error_rx: Receiver<String>,
    handle: Option<JoinHandle<Result<(), ApplicationRuntimeError>>>,
}

impl ProductStorageWriter {
    fn spawn(path: PathBuf) -> Result<Self, ApplicationRuntimeError> {
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(4);
        let (error_tx, error_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let handle = std::thread::Builder::new()
            .name("domainflow-product-db-writer".to_string())
            .spawn(move || {
                let mut domain_storage = match Storage::open(&path) {
                    Ok(storage) => storage,
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        return Err(ApplicationRuntimeError::WriterFailed(message));
                    }
                };
                let mut application_storage = match ApplicationStorage::open(&path) {
                    Ok(storage) => storage,
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        return Err(ApplicationRuntimeError::WriterFailed(message));
                    }
                };
                let _ = ready_tx.send(Ok(()));

                loop {
                    match command_rx.recv() {
                        Ok(WriterCommand::Write(domain_batch, application_batch)) => {
                            let result = domain_storage
                                .upsert_batch(&domain_batch)
                                .map_err(ApplicationRuntimeError::Storage)
                                .and_then(|()| {
                                    application_storage
                                        .upsert_batch(&application_batch)
                                        .map_err(ApplicationRuntimeError::ApplicationStorage)
                                });
                            if let Err(error) = result {
                                let message = error.to_string();
                                let _ = error_tx.send(message.clone());
                                return Err(ApplicationRuntimeError::WriterFailed(message));
                            }
                        }
                        Ok(WriterCommand::Shutdown) | Err(_) => return Ok(()),
                    }
                }
            })
            .map_err(|error| ApplicationRuntimeError::WriterFailed(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: command_tx,
                error_rx,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(ApplicationRuntimeError::WriterFailed(error))
            }
            Err(_) => {
                let _ = handle.join();
                Err(ApplicationRuntimeError::WriterChannelClosed)
            }
        }
    }

    fn submit(
        &self,
        domain_batch: FlushBatch,
        application_batch: ApplicationFlushBatch,
    ) -> Result<(), ApplicationRuntimeError> {
        if domain_batch.is_empty() && application_batch.is_empty() {
            return Ok(());
        }
        if let Ok(error) = self.error_rx.try_recv() {
            return Err(ApplicationRuntimeError::WriterFailed(error));
        }
        self.tx
            .send(WriterCommand::Write(domain_batch, application_batch))
            .map_err(|_| ApplicationRuntimeError::WriterChannelClosed)
    }

    fn poll_error(&self) -> Option<String> {
        self.error_rx.try_recv().ok()
    }

    fn shutdown(mut self) -> Result<(), ApplicationRuntimeError> {
        let reported_error = self.error_rx.try_recv().ok();
        let _ = self.tx.send(WriterCommand::Shutdown);
        let Some(handle) = self.handle.take() else {
            return Err(ApplicationRuntimeError::WriterPanicked);
        };
        match handle.join() {
            Ok(Ok(())) => reported_error
                .map(ApplicationRuntimeError::WriterFailed)
                .map_or(Ok(()), Err),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(ApplicationRuntimeError::WriterPanicked),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{AttributionError, ProcessIdentity};
    use crate::capture::OwnedPacket;
    use pcap::Linktype;
    use std::collections::VecDeque;
    use std::net::IpAddr;

    struct FakeAttributor;

    impl ProcessAttributor for FakeAttributor {
        fn lookup(
            &mut self,
            _flow: &crate::model::FlowKey,
        ) -> Result<Option<ProcessIdentity>, AttributionError> {
            Ok(Some(ProcessIdentity {
                pid: 42,
                name: "browser.exe".to_string(),
            }))
        }

        fn backend_name(&self) -> &'static str {
            "fake"
        }
    }

    struct FakeSource {
        events: VecDeque<Result<CaptureRead, CaptureError>>,
    }

    impl PacketSource for FakeSource {
        fn linktype(&self) -> Linktype {
            Linktype::ETHERNET
        }

        fn next_packet(&mut self) -> Result<CaptureRead, CaptureError> {
            self.events
                .pop_front()
                .unwrap_or(Ok(CaptureRead::EndOfFile))
        }
    }

    fn udp_packet() -> OwnedPacket {
        let payload = b"quic";
        let builder = etherparse::PacketBuilder::ethernet2(
            [0, 1, 2, 3, 4, 5],
            [6, 7, 8, 9, 10, 11],
        )
        .ipv4([10, 0, 0, 1], [1, 1, 1, 1], 64)
        .udp(50_000, 443);
        let mut frame = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut frame, payload).unwrap();
        OwnedPacket {
            timestamp_micros: 1_700_000_000_000_000,
            wire_len: u32::try_from(frame.len()).unwrap(),
            captured: frame.into(),
            linktype: Linktype::ETHERNET,
        }
    }

    fn temp_db() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "product_runtime_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path.set_extension("db");
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn attributed_udp_is_persisted_by_application() {
        let path = temp_db();
        let mut source = FakeSource {
            events: VecDeque::from([
                Ok(CaptureRead::Packet(udp_packet())),
                Ok(CaptureRead::EndOfFile),
            ]),
        };
        let writer = ProductStorageWriter::spawn(path.clone()).unwrap();
        let shutdown = AtomicBool::new(false);
        let mut attributor = FakeAttributor;
        let summary = run_source(
            &mut source,
            writer,
            &RuntimeConfig::default(),
            &shutdown,
            &mut attributor,
        )
        .unwrap();

        assert_eq!(summary.attributed_packets, 1);
        let storage = ApplicationStorage::open(&path).unwrap();
        let applications = storage
            .top_applications(crate::app_storage::TrafficPeriod::All, 10)
            .unwrap();
        assert_eq!(applications[0].application, "browser.exe");
        assert!(applications[0].bytes > 0);
        cleanup(&path);
    }

    #[test]
    fn lookup_summary_tracks_misses() {
        struct Missing;
        impl ProcessAttributor for Missing {
            fn lookup(
                &mut self,
                _flow: &crate::model::FlowKey,
            ) -> Result<Option<ProcessIdentity>, AttributionError> {
                Ok(None)
            }
            fn backend_name(&self) -> &'static str {
                "missing"
            }
        }

        let flow = crate::model::FlowKey::canonical(
            crate::model::TransportProtocol::Tcp,
            crate::model::Endpoint {
                ip: IpAddr::from([10, 0, 0, 1]),
                port: 50_000,
            },
            crate::model::Endpoint {
                ip: IpAddr::from([1, 1, 1, 1]),
                port: 443,
            },
        );
        let mut summary = ApplicationRunSummary::default();
        assert_eq!(
            lookup_application(&mut Missing, &flow, &mut summary),
            UNKNOWN_APPLICATION
        );
        assert_eq!(summary.attribution_misses, 1);
    }
}
