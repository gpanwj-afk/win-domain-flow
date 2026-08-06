use crate::model::{FlushBatch, TopDomainRow};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;
use thiserror::Error;

pub const SCHEMA_VERSION: i64 = 1;

const SQLITE_SYNCHRONOUS_NORMAL: i64 = 1;

const SCHEMA_SQL: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

INSERT INTO schema_meta (key, value)
VALUES ('schema_version', '1')
ON CONFLICT(key) DO NOTHING;

CREATE TABLE IF NOT EXISTS domain_daily (
    day_start_utc INTEGER NOT NULL
        CHECK (day_start_utc % 86400 = 0),
    domain TEXT NOT NULL
        CHECK (domain <> ''),
    bytes INTEGER NOT NULL
        CHECK (typeof(bytes) = 'integer' AND bytes >= 0),
    packets INTEGER NOT NULL
        CHECK (typeof(packets) = 'integer' AND packets >= 0),
    updated_at_utc INTEGER NOT NULL
        CHECK (typeof(updated_at_utc) = 'integer' AND updated_at_utc >= 0),
    PRIMARY KEY (day_start_utc, domain)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_domain_daily_domain_day
    ON domain_daily(domain, day_start_utc);

CREATE INDEX IF NOT EXISTS idx_domain_daily_day_bytes
    ON domain_daily(day_start_utc, bytes DESC);
";

const UPSERT_SQL: &str = "
INSERT INTO domain_daily (
    day_start_utc,
    domain,
    bytes,
    packets,
    updated_at_utc
)
VALUES (
    ?1,
    ?2,
    ?3,
    ?4,
    CAST(strftime('%s','now') AS INTEGER)
)
ON CONFLICT(day_start_utc, domain) DO UPDATE SET
    bytes = domain_daily.bytes + excluded.bytes,
    packets = domain_daily.packets + excluded.packets,
    updated_at_utc = CAST(strftime('%s','now') AS INTEGER);
";

const TOP_RECENT_SQL: &str = "
SELECT
    domain,
    SUM(bytes) AS total_bytes,
    SUM(packets) AS total_packets
FROM domain_daily
WHERE day_start_utc >= (
    (CAST(strftime('%s','now') AS INTEGER) / 86400) * 86400
    - ((?1 - 1) * 86400)
)
GROUP BY domain
ORDER BY total_bytes DESC, domain ASC
LIMIT ?2;
";

const TOP_SINCE_SQL: &str = "
SELECT
    domain,
    SUM(bytes) AS total_bytes,
    SUM(packets) AS total_packets
FROM domain_daily
WHERE day_start_utc >= ?1
GROUP BY domain
ORDER BY total_bytes DESC, domain ASC
LIMIT ?2;
";

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database path has no usable parent: {0}")]
    InvalidPath(PathBuf),

    #[error("counter for domain {domain} exceeds SQLite INTEGER range")]
    CounterTooLarge { domain: String },

    #[error("invalid query argument: {0}")]
    InvalidQuery(&'static str),

    #[error("invalid schema version value: {0}")]
    InvalidSchemaVersion(String),

    #[error("unsupported schema version: {0}")]
    UnsupportedSchemaVersion(i64),

    #[error("database pragma mismatch: {0}")]
    PragmaMismatch(&'static str),

    #[error("database writer channel is closed")]
    WriterChannelClosed,

    #[error("database writer failed: {0}")]
    WriterFailed(String),

    #[error("database writer thread panicked")]
    WriterPanicked,
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if path.as_os_str().is_empty() {
            return Err(StorageError::InvalidPath(path.to_path_buf()));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA_SQL)?;
        validate_pragmas(&conn)?;
        validate_schema_version(&conn)?;

        Ok(Self { conn })
    }

    pub fn upsert_batch(&mut self, batch: &FlushBatch) -> Result<(), StorageError> {
        if batch.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut statement = tx.prepare(UPSERT_SQL)?;
            for row in &batch.rows {
                let bytes = i64::try_from(row.counters.bytes).map_err(|_| {
                    StorageError::CounterTooLarge {
                        domain: row.domain.clone(),
                    }
                })?;
                let packets = i64::try_from(row.counters.packets).map_err(|_| {
                    StorageError::CounterTooLarge {
                        domain: row.domain.clone(),
                    }
                })?;

                statement.execute(params![row.day_start_utc, row.domain, bytes, packets])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn top_domains_recent(
        &self,
        days: u32,
        limit: u32,
    ) -> Result<Vec<TopDomainRow>, StorageError> {
        if !(1..=3650).contains(&days) {
            return Err(StorageError::InvalidQuery("days must be in 1..=3650"));
        }
        if !(1..=1000).contains(&limit) {
            return Err(StorageError::InvalidQuery("limit must be in 1..=1000"));
        }

        query_top_rows(&self.conn, TOP_RECENT_SQL, i64::from(days), limit)
    }

    pub fn top_domains_since(
        &self,
        min_day_start_utc: i64,
        limit: u32,
    ) -> Result<Vec<TopDomainRow>, StorageError> {
        if min_day_start_utc % 86_400 != 0 {
            return Err(StorageError::InvalidQuery(
                "min_day_start_utc must be divisible by 86400",
            ));
        }
        if !(1..=1000).contains(&limit) {
            return Err(StorageError::InvalidQuery("limit must be in 1..=1000"));
        }

        query_top_rows(&self.conn, TOP_SINCE_SQL, min_day_start_utc, limit)
    }

    pub fn schema_version(&self) -> Result<i64, StorageError> {
        read_schema_version(&self.conn)
    }
}

fn validate_pragmas(conn: &Connection) -> Result<(), StorageError> {
    let journal_mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StorageError::PragmaMismatch("journal_mode is not WAL"));
    }

    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if synchronous != SQLITE_SYNCHRONOUS_NORMAL {
        return Err(StorageError::PragmaMismatch(
            "synchronous is not NORMAL",
        ));
    }

    Ok(())
}

fn read_schema_version(conn: &Connection) -> Result<i64, StorageError> {
    let version_text: String = conn
        .prepare("SELECT value FROM schema_meta WHERE key = 'schema_version'")?
        .query_row([], |row| row.get(0))
        .map_err(|_| StorageError::InvalidSchemaVersion("not found".to_string()))?;

    version_text
        .parse()
        .map_err(|_| StorageError::InvalidSchemaVersion(version_text))
}

fn validate_schema_version(conn: &Connection) -> Result<(), StorageError> {
    let version = read_schema_version(conn)?;
    if version != SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion(version));
    }
    Ok(())
}

fn query_top_rows(
    conn: &Connection,
    sql: &str,
    lower_bound: i64,
    limit: u32,
) -> Result<Vec<TopDomainRow>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement
        .query_map(params![lower_bound, limit], |row| {
            let bytes = row.get::<_, i64>(1)?;
            let packets = row.get::<_, i64>(2)?;
            Ok(TopDomainRow {
                domain: row.get(0)?,
                bytes: u64::try_from(bytes).unwrap_or(0),
                packets: u64::try_from(packets).unwrap_or(0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

enum WriterCommand {
    Write(FlushBatch),
    Shutdown,
}

pub struct StorageWriter {
    tx: SyncSender<WriterCommand>,
    error_rx: Receiver<String>,
    handle: Option<JoinHandle<Result<(), StorageError>>>,
}

impl StorageWriter {
    pub fn spawn(path: PathBuf) -> Result<Self, StorageError> {
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(4);
        let (error_tx, error_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let handle = std::thread::Builder::new()
            .name("domainflow-db-writer".to_string())
            .spawn(move || {
                let mut storage = match Storage::open(&path) {
                    Ok(storage) => {
                        let _ = ready_tx.send(Ok(()));
                        storage
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        return Err(StorageError::WriterFailed(message));
                    }
                };

                loop {
                    match command_rx.recv() {
                        Ok(WriterCommand::Write(batch)) => {
                            if let Err(error) = storage.upsert_batch(&batch) {
                                let message = error.to_string();
                                let _ = error_tx.send(message.clone());
                                return Err(StorageError::WriterFailed(message));
                            }
                        }
                        Ok(WriterCommand::Shutdown) | Err(_) => return Ok(()),
                    }
                }
            })
            .map_err(|error| StorageError::WriterFailed(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: command_tx,
                error_rx,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(StorageError::WriterFailed(error))
            }
            Err(_) => {
                let _ = handle.join();
                Err(StorageError::WriterChannelClosed)
            }
        }
    }

    pub fn submit(&self, batch: FlushBatch) -> Result<(), StorageError> {
        if batch.is_empty() {
            return Ok(());
        }

        if let Ok(error) = self.error_rx.try_recv() {
            return Err(StorageError::WriterFailed(error));
        }

        self.tx
            .send(WriterCommand::Write(batch))
            .map_err(|_| StorageError::WriterChannelClosed)
    }

    pub fn poll_error(&self) -> Option<String> {
        self.error_rx.try_recv().ok()
    }

    pub fn shutdown(mut self) -> Result<(), StorageError> {
        let reported_error = self.error_rx.try_recv().ok();
        let _ = self.tx.send(WriterCommand::Shutdown);

        let Some(handle) = self.handle.take() else {
            return Err(StorageError::WriterPanicked);
        };

        match handle.join() {
            Ok(Ok(())) => {
                if let Some(error) = reported_error {
                    Err(StorageError::WriterFailed(error))
                } else {
                    Ok(())
                }
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(StorageError::WriterPanicked),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Counters, DomainDelta};
    use std::fs;

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let id = format!(
            "storage_test_{}_{}_{}",
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

    fn cleanup_db(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("db-wal"));
        let _ = fs::remove_file(path.with_extension("db-shm"));
    }

    fn delta(day_start_utc: i64, domain: &str, bytes: u64, packets: u64) -> DomainDelta {
        DomainDelta {
            day_start_utc,
            domain: domain.to_string(),
            counters: Counters { bytes, packets },
        }
    }

    #[test]
    fn schema_is_version_one_wal_and_normal_synchronous() {
        let path = temp_db_path("schema");
        let storage = Storage::open(&path).unwrap();

        assert_eq!(storage.schema_version().unwrap(), 1);
        let journal_mode: String = storage
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = storage
            .conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        assert_eq!(synchronous, SQLITE_SYNCHRONOUS_NORMAL);

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn upsert_is_additive() {
        let path = temp_db_path("upsert_add");
        let mut storage = Storage::open(&path).unwrap();

        storage
            .upsert_batch(&FlushBatch {
                rows: vec![delta(86_400, "example.com", 100, 1)],
            })
            .unwrap();
        storage
            .upsert_batch(&FlushBatch {
                rows: vec![delta(86_400, "example.com", 50, 2)],
            })
            .unwrap();

        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 150);
        assert_eq!(rows[0].packets, 3);

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn upsert_separates_days() {
        let path = temp_db_path("upsert_days");
        let mut storage = Storage::open(&path).unwrap();
        storage
            .upsert_batch(&FlushBatch {
                rows: vec![
                    delta(86_400, "example.com", 100, 1),
                    delta(172_800, "example.com", 200, 2),
                ],
            })
            .unwrap();

        let count: i64 = storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM domain_daily WHERE domain = 'example.com'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);

        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows[0].bytes, 300);
        assert_eq!(rows[0].packets, 3);

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn top_domains_orders_equal_bytes_by_domain() {
        let path = temp_db_path("top_order");
        let mut storage = Storage::open(&path).unwrap();
        storage
            .upsert_batch(&FlushBatch {
                rows: vec![
                    delta(86_400, "b.com", 200, 2),
                    delta(86_400, "a.com", 200, 1),
                    delta(86_400, "c.com", 150, 1),
                ],
            })
            .unwrap();

        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].domain, "a.com");
        assert_eq!(rows[1].domain, "b.com");
        assert_eq!(rows[2].domain, "c.com");

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn empty_batch_is_noop() {
        let path = temp_db_path("empty_batch");
        let mut storage = Storage::open(&path).unwrap();
        storage.upsert_batch(&FlushBatch { rows: Vec::new() }).unwrap();
        assert!(storage.top_domains_since(0, 10).unwrap().is_empty());

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn writer_flushes_before_shutdown() {
        let path = temp_db_path("writer_flush");
        let writer = StorageWriter::spawn(path.clone()).unwrap();
        writer
            .submit(FlushBatch {
                rows: vec![delta(86_400, "example.com", 100, 1)],
            })
            .unwrap();
        writer.shutdown().unwrap();

        let storage = Storage::open(&path).unwrap();
        let rows = storage.top_domains_since(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 100);

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn counter_larger_than_sqlite_integer_is_rejected() {
        let path = temp_db_path("counter_limit");
        let mut storage = Storage::open(&path).unwrap();
        let result = storage.upsert_batch(&FlushBatch {
            rows: vec![delta(86_400, "example.com", u64::MAX, 1)],
        });

        assert!(matches!(
            result,
            Err(StorageError::CounterTooLarge { .. })
        ));

        drop(storage);
        cleanup_db(&path);
    }

    #[test]
    fn invalid_query_arguments_are_rejected() {
        let path = temp_db_path("invalid_query");
        let storage = Storage::open(&path).unwrap();

        assert!(matches!(
            storage.top_domains_recent(0, 10),
            Err(StorageError::InvalidQuery(_))
        ));
        assert!(matches!(
            storage.top_domains_recent(1, 0),
            Err(StorageError::InvalidQuery(_))
        ));
        assert!(matches!(
            storage.top_domains_since(1, 10),
            Err(StorageError::InvalidQuery(_))
        ));
        assert!(matches!(
            storage.top_domains_since(0, 0),
            Err(StorageError::InvalidQuery(_))
        ));

        drop(storage);
        cleanup_db(&path);
    }
}
