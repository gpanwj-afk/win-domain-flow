use crate::model::{
    ApplicationFlushBatch, TopApplicationRow, TopDomainRow, TrafficTotals, HISTORICAL_APPLICATION,
    UNKNOWN_APPLICATION, UNKNOWN_DOMAIN,
};
use crate::storage::{Storage, StorageError};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;
use thiserror::Error;

const APP_SCHEMA_VERSION: &str = "1";
const APP_SCHEMA_META_KEY: &str = "application_schema_version";
const SQLITE_SYNCHRONOUS_NORMAL: i64 = 1;

const APP_SCHEMA_SQL: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS application_domain_daily (
    day_start_utc INTEGER NOT NULL
        CHECK (day_start_utc % 86400 = 0),
    application TEXT NOT NULL
        CHECK (application <> ''),
    domain TEXT NOT NULL
        CHECK (domain <> ''),
    bytes INTEGER NOT NULL
        CHECK (typeof(bytes) = 'integer' AND bytes >= 0),
    packets INTEGER NOT NULL
        CHECK (typeof(packets) = 'integer' AND packets >= 0),
    updated_at_utc INTEGER NOT NULL
        CHECK (typeof(updated_at_utc) = 'integer' AND updated_at_utc >= 0),
    PRIMARY KEY (day_start_utc, application, domain)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_application_daily_period_bytes
    ON application_domain_daily(day_start_utc, bytes DESC);

CREATE INDEX IF NOT EXISTS idx_application_daily_app_period
    ON application_domain_daily(application, day_start_utc, bytes DESC);

CREATE INDEX IF NOT EXISTS idx_application_daily_domain_period
    ON application_domain_daily(domain, day_start_utc, bytes DESC);
";

const UPSERT_SQL: &str = "
INSERT INTO application_domain_daily (
    day_start_utc,
    application,
    domain,
    bytes,
    packets,
    updated_at_utc
)
VALUES (?1, ?2, ?3, ?4, ?5, CAST(strftime('%s','now') AS INTEGER))
ON CONFLICT(day_start_utc, application, domain) DO UPDATE SET
    bytes = application_domain_daily.bytes + excluded.bytes,
    packets = application_domain_daily.packets + excluded.packets,
    updated_at_utc = CAST(strftime('%s','now') AS INTEGER);
";

const TOP_APPLICATIONS_SQL: &str = "
SELECT application, SUM(bytes), SUM(packets)
FROM application_domain_daily
WHERE day_start_utc >= ?1
GROUP BY application
ORDER BY SUM(bytes) DESC, application ASC
LIMIT ?2;
";

const TOP_DOMAINS_ALL_SQL: &str = "
SELECT domain, SUM(bytes), SUM(packets)
FROM application_domain_daily
WHERE day_start_utc >= ?1
GROUP BY domain
ORDER BY SUM(bytes) DESC, domain ASC
LIMIT ?2;
";

const TOP_DOMAINS_FOR_APPLICATION_SQL: &str = "
SELECT domain, SUM(bytes), SUM(packets)
FROM application_domain_daily
WHERE day_start_utc >= ?1 AND application = ?2
GROUP BY domain
ORDER BY SUM(bytes) DESC, domain ASC
LIMIT ?3;
";

const TOTALS_SQL: &str = "
SELECT
    COALESCE(SUM(bytes), 0),
    COALESCE(SUM(packets), 0),
    COALESCE(SUM(CASE WHEN domain = ?2 THEN bytes ELSE 0 END), 0),
    COALESCE(SUM(CASE WHEN application IN (?3, ?4) THEN bytes ELSE 0 END), 0)
FROM application_domain_daily
WHERE day_start_utc >= ?1;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficPeriod {
    Today,
    MonthToDate,
    Last7Days,
    Last30Days,
    All,
}

impl TrafficPeriod {
    pub const ALL: [Self; 5] = [
        Self::Today,
        Self::MonthToDate,
        Self::Last7Days,
        Self::Last30Days,
        Self::All,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::MonthToDate => "month",
            Self::Last7Days => "7days",
            Self::Last30Days => "30days",
            Self::All => "all",
        }
    }

    pub fn from_key(value: &str) -> Self {
        match value {
            "today" => Self::Today,
            "7days" => Self::Last7Days,
            "30days" => Self::Last30Days,
            "all" => Self::All,
            _ => Self::MonthToDate,
        }
    }
}

#[derive(Debug, Error)]
pub enum ApplicationStorageError {
    #[error(transparent)]
    DomainStorage(#[from] StorageError),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("application counter for {application} / {domain} exceeds SQLite INTEGER range")]
    CounterTooLarge { application: String, domain: String },

    #[error("invalid application query argument: {0}")]
    InvalidQuery(&'static str),

    #[error("application database pragma mismatch: {0}")]
    PragmaMismatch(&'static str),

    #[error("application database writer channel is closed")]
    WriterChannelClosed,

    #[error("application database writer failed: {0}")]
    WriterFailed(String),

    #[error("application database writer thread panicked")]
    WriterPanicked,
}

pub struct ApplicationStorage {
    conn: Connection,
}

impl ApplicationStorage {
    pub fn open(path: &Path) -> Result<Self, ApplicationStorageError> {
        drop(Storage::open(path)?);

        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(APP_SCHEMA_SQL)?;
        validate_pragmas(&conn)?;
        migrate_historical_rows(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn upsert_batch(
        &mut self,
        batch: &ApplicationFlushBatch,
    ) -> Result<(), ApplicationStorageError> {
        if batch.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut statement = tx.prepare(UPSERT_SQL)?;
            for row in &batch.rows {
                let bytes = i64::try_from(row.counters.bytes).map_err(|_| {
                    ApplicationStorageError::CounterTooLarge {
                        application: row.application.clone(),
                        domain: row.domain.clone(),
                    }
                })?;
                let packets = i64::try_from(row.counters.packets).map_err(|_| {
                    ApplicationStorageError::CounterTooLarge {
                        application: row.application.clone(),
                        domain: row.domain.clone(),
                    }
                })?;
                statement.execute(params![
                    row.day_start_utc,
                    row.application,
                    row.domain,
                    bytes,
                    packets
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn top_applications(
        &self,
        period: TrafficPeriod,
        limit: u32,
    ) -> Result<Vec<TopApplicationRow>, ApplicationStorageError> {
        validate_limit(limit)?;
        let lower_bound = self.period_start_utc(period)?;
        let mut statement = self.conn.prepare(TOP_APPLICATIONS_SQL)?;
        let rows = statement
            .query_map(params![lower_bound, i64::from(limit)], |row| {
                Ok(TopApplicationRow {
                    application: row.get(0)?,
                    bytes: nonnegative_u64(row.get(1)?),
                    packets: nonnegative_u64(row.get(2)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn top_domains(
        &self,
        period: TrafficPeriod,
        application: Option<&str>,
        limit: u32,
    ) -> Result<Vec<TopDomainRow>, ApplicationStorageError> {
        validate_limit(limit)?;
        let lower_bound = self.period_start_utc(period)?;

        let rows = if let Some(application) = application {
            let mut statement = self.conn.prepare(TOP_DOMAINS_FOR_APPLICATION_SQL)?;
            statement
                .query_map(
                    params![lower_bound, application, i64::from(limit)],
                    map_domain_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = self.conn.prepare(TOP_DOMAINS_ALL_SQL)?;
            statement
                .query_map(params![lower_bound, i64::from(limit)], map_domain_row)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    pub fn totals(&self, period: TrafficPeriod) -> Result<TrafficTotals, ApplicationStorageError> {
        let lower_bound = self.period_start_utc(period)?;
        self.conn
            .prepare(TOTALS_SQL)?
            .query_row(
                params![
                    lower_bound,
                    UNKNOWN_DOMAIN,
                    UNKNOWN_APPLICATION,
                    HISTORICAL_APPLICATION
                ],
                |row| {
                    Ok(TrafficTotals {
                        bytes: nonnegative_u64(row.get(0)?),
                        packets: nonnegative_u64(row.get(1)?),
                        unknown_domain_bytes: nonnegative_u64(row.get(2)?),
                        unknown_application_bytes: nonnegative_u64(row.get(3)?),
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn period_start_utc(&self, period: TrafficPeriod) -> Result<i64, ApplicationStorageError> {
        let sql = match period {
            TrafficPeriod::Today => {
                "SELECT (CAST(strftime('%s','now') AS INTEGER) / 86400) * 86400"
            }
            TrafficPeriod::MonthToDate => {
                "SELECT (CAST(strftime('%s', strftime('%Y-%m-01 00:00:00','now','localtime'), 'utc') AS INTEGER) / 86400) * 86400"
            }
            TrafficPeriod::Last7Days => {
                "SELECT ((CAST(strftime('%s','now') AS INTEGER) / 86400) * 86400) - (6 * 86400)"
            }
            TrafficPeriod::Last30Days => {
                "SELECT ((CAST(strftime('%s','now') AS INTEGER) / 86400) * 86400) - (29 * 86400)"
            }
            TrafficPeriod::All => return Ok(0),
        };
        self.conn
            .query_row(sql, [], |row| row.get(0))
            .map_err(Into::into)
    }
}

fn validate_pragmas(conn: &Connection) -> Result<(), ApplicationStorageError> {
    let journal_mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(ApplicationStorageError::PragmaMismatch(
            "journal_mode is not WAL",
        ));
    }
    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if synchronous != SQLITE_SYNCHRONOUS_NORMAL {
        return Err(ApplicationStorageError::PragmaMismatch(
            "synchronous is not NORMAL",
        ));
    }
    Ok(())
}

fn migrate_historical_rows(conn: &mut Connection) -> Result<(), ApplicationStorageError> {
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = ?1",
            [APP_SCHEMA_META_KEY],
            |row| row.get(0),
        )
        .optional()?;

    if current.as_deref() == Some(APP_SCHEMA_VERSION) {
        return Ok(());
    }

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO application_domain_daily (
            day_start_utc, application, domain, bytes, packets, updated_at_utc
         )
         SELECT day_start_utc, ?1, domain, bytes, packets, updated_at_utc
         FROM domain_daily
         ON CONFLICT(day_start_utc, application, domain) DO NOTHING",
        [HISTORICAL_APPLICATION],
    )?;
    tx.execute(
        "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![APP_SCHEMA_META_KEY, APP_SCHEMA_VERSION],
    )?;
    tx.commit()?;
    Ok(())
}

fn validate_limit(limit: u32) -> Result<(), ApplicationStorageError> {
    if !(1..=1000).contains(&limit) {
        return Err(ApplicationStorageError::InvalidQuery(
            "limit must be in 1..=1000",
        ));
    }
    Ok(())
}

fn map_domain_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TopDomainRow> {
    Ok(TopDomainRow {
        domain: row.get(0)?,
        bytes: nonnegative_u64(row.get(1)?),
        packets: nonnegative_u64(row.get(2)?),
    })
}

fn nonnegative_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

enum WriterCommand {
    Write(ApplicationFlushBatch),
    Shutdown,
}

pub struct ApplicationStorageWriter {
    tx: SyncSender<WriterCommand>,
    error_rx: Receiver<String>,
    handle: Option<JoinHandle<Result<(), ApplicationStorageError>>>,
}

impl ApplicationStorageWriter {
    pub fn spawn(path: PathBuf) -> Result<Self, ApplicationStorageError> {
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(4);
        let (error_tx, error_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let handle = std::thread::Builder::new()
            .name("domainflow-application-db-writer".to_string())
            .spawn(move || {
                let mut storage = match ApplicationStorage::open(&path) {
                    Ok(storage) => {
                        let _ = ready_tx.send(Ok(()));
                        storage
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        return Err(ApplicationStorageError::WriterFailed(message));
                    }
                };

                loop {
                    match command_rx.recv() {
                        Ok(WriterCommand::Write(batch)) => {
                            if let Err(error) = storage.upsert_batch(&batch) {
                                let message = error.to_string();
                                let _ = error_tx.send(message.clone());
                                return Err(ApplicationStorageError::WriterFailed(message));
                            }
                        }
                        Ok(WriterCommand::Shutdown) | Err(_) => return Ok(()),
                    }
                }
            })
            .map_err(|error| ApplicationStorageError::WriterFailed(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: command_tx,
                error_rx,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(ApplicationStorageError::WriterFailed(error))
            }
            Err(_) => {
                let _ = handle.join();
                Err(ApplicationStorageError::WriterChannelClosed)
            }
        }
    }

    pub fn submit(&self, batch: ApplicationFlushBatch) -> Result<(), ApplicationStorageError> {
        if batch.is_empty() {
            return Ok(());
        }
        if let Ok(error) = self.error_rx.try_recv() {
            return Err(ApplicationStorageError::WriterFailed(error));
        }
        self.tx
            .send(WriterCommand::Write(batch))
            .map_err(|_| ApplicationStorageError::WriterChannelClosed)
    }

    pub fn poll_error(&self) -> Option<String> {
        self.error_rx.try_recv().ok()
    }

    pub fn shutdown(mut self) -> Result<(), ApplicationStorageError> {
        let reported_error = self.error_rx.try_recv().ok();
        let _ = self.tx.send(WriterCommand::Shutdown);
        let Some(handle) = self.handle.take() else {
            return Err(ApplicationStorageError::WriterPanicked);
        };
        match handle.join() {
            Ok(Ok(())) => reported_error
                .map(ApplicationStorageError::WriterFailed)
                .map_or(Ok(()), Err),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(ApplicationStorageError::WriterPanicked),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ApplicationDomainDelta, ApplicationFlushBatch, Counters, DomainDelta, FlushBatch,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "app_storage_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            name
        ));
        path.set_extension("db");
        path
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("db-wal"));
        let _ = fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn legacy_domain_rows_are_visible_as_historical_application() {
        let path = temp_db_path("migration");
        let mut domain_storage = Storage::open(&path).unwrap();
        domain_storage
            .upsert_batch(&FlushBatch {
                rows: vec![DomainDelta {
                    day_start_utc: 86_400,
                    domain: "example.com".to_string(),
                    counters: Counters {
                        bytes: 100,
                        packets: 1,
                    },
                }],
            })
            .unwrap();
        drop(domain_storage);

        let storage = ApplicationStorage::open(&path).unwrap();
        let apps = storage.top_applications(TrafficPeriod::All, 10).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].application, HISTORICAL_APPLICATION);
        assert_eq!(apps[0].bytes, 100);
        cleanup(&path);
    }

    #[test]
    fn application_rows_are_additive_and_queryable() {
        let path = temp_db_path("additive");
        let mut storage = ApplicationStorage::open(&path).unwrap();
        let row = |bytes| ApplicationDomainDelta {
            day_start_utc: 86_400,
            application: "chrome.exe".to_string(),
            domain: "example.com".to_string(),
            counters: Counters { bytes, packets: 1 },
        };
        storage
            .upsert_batch(&ApplicationFlushBatch {
                rows: vec![row(100), row(50)],
            })
            .unwrap();

        let apps = storage.top_applications(TrafficPeriod::All, 10).unwrap();
        let domains = storage
            .top_domains(TrafficPeriod::All, Some("chrome.exe"), 10)
            .unwrap();
        assert_eq!(apps[0].bytes, 150);
        assert_eq!(domains[0].bytes, 150);
        assert_eq!(storage.totals(TrafficPeriod::All).unwrap().packets, 2);
        cleanup(&path);
    }

    #[test]
    fn writer_flushes_before_shutdown() {
        let path = temp_db_path("writer");
        let writer = ApplicationStorageWriter::spawn(path.clone()).unwrap();
        writer
            .submit(ApplicationFlushBatch {
                rows: vec![ApplicationDomainDelta {
                    day_start_utc: 86_400,
                    application: "app.exe".to_string(),
                    domain: "example.com".to_string(),
                    counters: Counters {
                        bytes: 42,
                        packets: 1,
                    },
                }],
            })
            .unwrap();
        writer.shutdown().unwrap();
        let storage = ApplicationStorage::open(&path).unwrap();
        assert_eq!(storage.totals(TrafficPeriod::All).unwrap().bytes, 42);
        cleanup(&path);
    }
}
