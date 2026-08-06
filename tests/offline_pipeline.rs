use std::path::PathBuf;
use std::time::Duration;
use win_domain_flow::model::DEFAULT_BPF_FILTER;
use win_domain_flow::runtime::{run_offline, RuntimeConfig};
use win_domain_flow::storage::Storage;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("example_tls.pcap")
}

fn temp_db_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let id = format!(
        "offline_test_{}_{}_{}",
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

fn cleanup_db(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    if let Some(parent) = path.parent() {
        if parent != std::env::temp_dir() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

#[test]
fn offline_pcap_extracts_sni_and_counts_bidirectional_bytes() {
    let db_path = temp_db_path("sni_bytes");
    let fixture = fixture_path();

    let config = RuntimeConfig {
        flush_interval: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(300),
        bpf_filter: DEFAULT_BPF_FILTER.to_string(),
    };

    let _summary = run_offline(&fixture, &db_path, config).unwrap();

    let storage = Storage::open(&db_path).unwrap();
    let rows = storage.top_domains_since(0, 10).unwrap();

    let example_row = rows.iter().find(|r| r.domain == "example.com");
    assert!(
        example_row.is_some(),
        "example.com not found in top domains"
    );

    let row = example_row.unwrap();
    assert_eq!(row.bytes, 190, "example.com bytes mismatch");
    assert_eq!(row.packets, 2, "example.com packets mismatch");

    drop(storage);
    cleanup_db(&db_path);
}

#[test]
fn offline_pcap_counts_udp_443_as_unknown() {
    let db_path = temp_db_path("udp_unknown");
    let fixture = fixture_path();

    let config = RuntimeConfig {
        flush_interval: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(300),
        bpf_filter: DEFAULT_BPF_FILTER.to_string(),
    };

    let _summary = run_offline(&fixture, &db_path, config).unwrap();

    let storage = Storage::open(&db_path).unwrap();
    let rows = storage.top_domains_since(0, 10).unwrap();

    let unknown_row = rows.iter().find(|r| r.domain == "(unknown)");
    assert!(unknown_row.is_some(), "(unknown) not found in top domains");

    let row = unknown_row.unwrap();
    assert_eq!(row.bytes, 54, "(unknown) bytes mismatch");
    assert_eq!(row.packets, 1, "(unknown) packets mismatch");

    drop(storage);
    cleanup_db(&db_path);
}

#[test]
fn offline_run_summary_is_stable() {
    let db_path = temp_db_path("summary");
    let fixture = fixture_path();

    let config = RuntimeConfig {
        flush_interval: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(300),
        bpf_filter: DEFAULT_BPF_FILTER.to_string(),
    };

    let summary = run_offline(&fixture, &db_path, config).unwrap();

    assert_eq!(summary.captured_packets, 3);
    assert_eq!(summary.accepted_packets, 3);
    assert_eq!(summary.skipped_packets, 0);
    assert_eq!(summary.parse_errors, 0);
    assert_eq!(summary.resolved_flows, 1);
    assert!(summary.submitted_batches >= 1);

    cleanup_db(&db_path);
}
