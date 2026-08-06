use crate::app_storage::{ApplicationStorage, TrafficPeriod};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const BROWSER_DIAGNOSTICS_PORT: u16 = 38_765;
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024;

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS browser_request_event (
    event_id TEXT PRIMARY KEY,
    occurred_at_ms INTEGER NOT NULL,
    host TEXT NOT NULL,
    url TEXT NOT NULL,
    page_url TEXT,
    initiator TEXT,
    method TEXT,
    resource_type TEXT,
    status_code INTEGER,
    mime TEXT,
    declared_bytes INTEGER,
    content_disposition TEXT,
    error_text TEXT
);

CREATE INDEX IF NOT EXISTS idx_browser_request_host_time
    ON browser_request_event(host, occurred_at_ms DESC);

CREATE INDEX IF NOT EXISTS idx_browser_request_page_time
    ON browser_request_event(page_url, occurred_at_ms DESC);

CREATE TABLE IF NOT EXISTS browser_download_event (
    event_id TEXT PRIMARY KEY,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    ended_at_ms INTEGER,
    host TEXT NOT NULL,
    url TEXT NOT NULL,
    final_url TEXT,
    filename TEXT,
    mime TEXT,
    total_bytes INTEGER,
    state TEXT,
    danger TEXT,
    exists_local INTEGER
);

CREATE INDEX IF NOT EXISTS idx_browser_download_host_time
    ON browser_download_event(host, updated_at_ms DESC);
"#;

#[derive(Debug, Error)]
pub enum BrowserActivityError {
    #[error("browser diagnostics I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("browser diagnostics SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("browser diagnostics JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("browser diagnostics protocol error: {0}")]
    Protocol(&'static str),

    #[error("browser diagnostics thread panicked")]
    ThreadPanicked,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserEventPayload {
    pub kind: String,
    pub event_id: String,
    pub timestamp_ms: i64,
    pub host: String,
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub initiator: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub status_code: Option<i64>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub declared_bytes: Option<i64>,
    #[serde(default)]
    pub content_disposition: Option<String>,
    #[serde(default)]
    pub error_text: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub total_bytes: Option<i64>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub danger: Option<String>,
    #[serde(default)]
    pub exists_local: Option<bool>,
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
}

impl BrowserEventPayload {
    fn normalize(mut self) -> Result<Self, BrowserActivityError> {
        self.kind = normalize_required(self.kind, "kind")?.to_ascii_lowercase();
        if self.kind != "request" && self.kind != "download" {
            return Err(BrowserActivityError::Protocol("unsupported event kind"));
        }
        self.event_id = normalize_required(self.event_id, "event_id")?;
        self.host = normalize_required(self.host, "host")?.to_ascii_lowercase();
        self.url = normalize_required(self.url, "url")?;
        self.final_url = normalize_optional(self.final_url);
        self.page_url = normalize_optional(self.page_url);
        self.initiator = normalize_optional(self.initiator);
        self.method = normalize_optional(self.method);
        self.resource_type = normalize_optional(self.resource_type);
        self.mime = normalize_optional(self.mime);
        self.content_disposition = normalize_optional(self.content_disposition);
        self.error_text = normalize_optional(self.error_text);
        self.filename = normalize_optional(self.filename);
        self.state = normalize_optional(self.state);
        self.danger = normalize_optional(self.danger);
        self.declared_bytes = nonnegative_optional(self.declared_bytes);
        self.total_bytes = nonnegative_optional(self.total_bytes);
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRequestRow {
    pub occurred_at_ms: i64,
    pub host: String,
    pub url: String,
    pub page_url: Option<String>,
    pub initiator: Option<String>,
    pub method: Option<String>,
    pub resource_type: Option<String>,
    pub status_code: Option<i64>,
    pub mime: Option<String>,
    pub declared_bytes: Option<u64>,
    pub content_disposition: Option<String>,
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDownloadRow {
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub host: String,
    pub url: String,
    pub final_url: Option<String>,
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub total_bytes: Option<u64>,
    pub state: Option<String>,
    pub danger: Option<String>,
    pub exists_local: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserResourceSummaryRow {
    pub resource_type: String,
    pub requests: u64,
    pub declared_bytes: u64,
    pub unknown_size_requests: u64,
}

pub struct BrowserActivityStorage {
    conn: Connection,
}

impl BrowserActivityStorage {
    pub fn open(path: &Path) -> Result<Self, BrowserActivityError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

    pub fn record(&mut self, payload: BrowserEventPayload) -> Result<(), BrowserActivityError> {
        let payload = payload.normalize()?;
        if payload.kind == "download" {
            self.record_download(&payload)
        } else {
            self.record_request(&payload)
        }
    }

    fn record_request(&mut self, payload: &BrowserEventPayload) -> Result<(), BrowserActivityError> {
        self.conn.execute(
            r#"INSERT INTO browser_request_event (
                event_id, occurred_at_ms, host, url, page_url, initiator, method,
                resource_type, status_code, mime, declared_bytes,
                content_disposition, error_text
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(event_id) DO UPDATE SET
                occurred_at_ms = excluded.occurred_at_ms,
                host = excluded.host,
                url = excluded.url,
                page_url = excluded.page_url,
                initiator = excluded.initiator,
                method = excluded.method,
                resource_type = excluded.resource_type,
                status_code = excluded.status_code,
                mime = excluded.mime,
                declared_bytes = excluded.declared_bytes,
                content_disposition = excluded.content_disposition,
                error_text = excluded.error_text"#,
            params![
                payload.event_id,
                payload.timestamp_ms,
                payload.host,
                payload.url,
                payload.page_url,
                payload.initiator,
                payload.method,
                payload.resource_type,
                payload.status_code,
                payload.mime,
                payload.declared_bytes,
                payload.content_disposition,
                payload.error_text,
            ],
        )?;
        Ok(())
    }

    fn record_download(&mut self, payload: &BrowserEventPayload) -> Result<(), BrowserActivityError> {
        self.conn.execute(
            r#"INSERT INTO browser_download_event (
                event_id, updated_at_ms, started_at_ms, ended_at_ms, host, url,
                final_url, filename, mime, total_bytes, state, danger, exists_local
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(event_id) DO UPDATE SET
                updated_at_ms = excluded.updated_at_ms,
                started_at_ms = COALESCE(excluded.started_at_ms, browser_download_event.started_at_ms),
                ended_at_ms = COALESCE(excluded.ended_at_ms, browser_download_event.ended_at_ms),
                host = excluded.host,
                url = excluded.url,
                final_url = COALESCE(excluded.final_url, browser_download_event.final_url),
                filename = COALESCE(excluded.filename, browser_download_event.filename),
                mime = COALESCE(excluded.mime, browser_download_event.mime),
                total_bytes = COALESCE(excluded.total_bytes, browser_download_event.total_bytes),
                state = COALESCE(excluded.state, browser_download_event.state),
                danger = COALESCE(excluded.danger, browser_download_event.danger),
                exists_local = COALESCE(excluded.exists_local, browser_download_event.exists_local)"#,
            params![
                payload.event_id,
                payload.timestamp_ms,
                payload.started_at_ms,
                payload.ended_at_ms,
                payload.host,
                payload.url,
                payload.final_url,
                payload.filename,
                payload.mime,
                payload.total_bytes,
                payload.state,
                payload.danger,
                payload.exists_local.map(i64::from),
            ],
        )?;
        Ok(())
    }

    pub fn recent_requests(
        &self,
        host_filter: Option<&str>,
        since_ms: i64,
        limit: u32,
    ) -> Result<Vec<BrowserRequestRow>, BrowserActivityError> {
        let host = normalized_filter(host_filter);
        let mut statement = self.conn.prepare(
            r#"SELECT occurred_at_ms, host, url, page_url, initiator, method,
                      resource_type, status_code, mime, declared_bytes,
                      content_disposition, error_text
               FROM browser_request_event
               WHERE occurred_at_ms >= ?1
                 AND (?2 = '' OR host = ?2)
               ORDER BY occurred_at_ms DESC
               LIMIT ?3"#,
        )?;
        let rows = statement
            .query_map(params![since_ms, host, i64::from(limit.clamp(1, 500))], |row| {
                Ok(BrowserRequestRow {
                    occurred_at_ms: row.get(0)?,
                    host: row.get(1)?,
                    url: row.get(2)?,
                    page_url: row.get(3)?,
                    initiator: row.get(4)?,
                    method: row.get(5)?,
                    resource_type: row.get(6)?,
                    status_code: row.get(7)?,
                    mime: row.get(8)?,
                    declared_bytes: optional_nonnegative(row.get(9)?),
                    content_disposition: row.get(10)?,
                    error_text: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn recent_downloads(
        &self,
        host_filter: Option<&str>,
        since_ms: i64,
        limit: u32,
    ) -> Result<Vec<BrowserDownloadRow>, BrowserActivityError> {
        let host = normalized_filter(host_filter);
        let mut statement = self.conn.prepare(
            r#"SELECT updated_at_ms, started_at_ms, ended_at_ms, host, url,
                      final_url, filename, mime, total_bytes, state, danger, exists_local
               FROM browser_download_event
               WHERE updated_at_ms >= ?1
                 AND (?2 = '' OR host = ?2)
               ORDER BY updated_at_ms DESC
               LIMIT ?3"#,
        )?;
        let rows = statement
            .query_map(params![since_ms, host, i64::from(limit.clamp(1, 200))], |row| {
                Ok(BrowserDownloadRow {
                    updated_at_ms: row.get(0)?,
                    started_at_ms: row.get(1)?,
                    ended_at_ms: row.get(2)?,
                    host: row.get(3)?,
                    url: row.get(4)?,
                    final_url: row.get(5)?,
                    filename: row.get(6)?,
                    mime: row.get(7)?,
                    total_bytes: optional_nonnegative(row.get(8)?),
                    state: row.get(9)?,
                    danger: row.get(10)?,
                    exists_local: row.get::<_, Option<i64>>(11)?.map(|value| value != 0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn resource_summary(
        &self,
        host_filter: Option<&str>,
        since_ms: i64,
        limit: u32,
    ) -> Result<Vec<BrowserResourceSummaryRow>, BrowserActivityError> {
        let host = normalized_filter(host_filter);
        let mut statement = self.conn.prepare(
            r#"SELECT COALESCE(NULLIF(resource_type, ''), 'other'),
                      COUNT(*),
                      COALESCE(SUM(COALESCE(declared_bytes, 0)), 0),
                      SUM(CASE WHEN declared_bytes IS NULL THEN 1 ELSE 0 END)
               FROM browser_request_event
               WHERE occurred_at_ms >= ?1
                 AND (?2 = '' OR host = ?2)
               GROUP BY COALESCE(NULLIF(resource_type, ''), 'other')
               ORDER BY SUM(COALESCE(declared_bytes, 0)) DESC, COUNT(*) DESC
               LIMIT ?3"#,
        )?;
        let rows = statement
            .query_map(params![since_ms, host, i64::from(limit.clamp(1, 50))], |row| {
                Ok(BrowserResourceSummaryRow {
                    resource_type: row.get(0)?,
                    requests: nonnegative(row.get(1)?),
                    declared_bytes: nonnegative(row.get(2)?),
                    unknown_size_requests: nonnegative(row.get(3)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn period_start_ms(
        database_path: &Path,
        period: TrafficPeriod,
    ) -> Result<i64, BrowserActivityError> {
        let storage = ApplicationStorage::open(database_path)
            .map_err(|error| BrowserActivityError::Io(std::io::Error::other(error)))?;
        Ok(storage.period_start_utc(period).map_err(|error| {
            BrowserActivityError::Io(std::io::Error::other(error))
        })? * 1_000)
    }
}

#[derive(Debug, Clone)]
pub struct BrowserServerStatus {
    pub listening: bool,
    pub accepted_events: u64,
    pub last_event_ms: Option<u64>,
    pub last_error: Option<String>,
}

pub struct BrowserActivityServer {
    shutdown: Arc<AtomicBool>,
    listening: Arc<AtomicBool>,
    accepted_events: Arc<AtomicU64>,
    last_event_ms: Arc<AtomicU64>,
    last_error: Arc<Mutex<Option<String>>>,
    database_path: Arc<RwLock<PathBuf>>,
    handle: Option<JoinHandle<()>>,
}

impl BrowserActivityServer {
    pub fn spawn(database_path: PathBuf) -> Result<Self, BrowserActivityError> {
        Self::spawn_on(database_path, BROWSER_DIAGNOSTICS_PORT)
    }

    fn spawn_on(database_path: PathBuf, port: u16) -> Result<Self, BrowserActivityError> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let listening = Arc::new(AtomicBool::new(false));
        let accepted_events = Arc::new(AtomicU64::new(0));
        let last_event_ms = Arc::new(AtomicU64::new(0));
        let last_error = Arc::new(Mutex::new(None));
        let database_path = Arc::new(RwLock::new(database_path));

        let worker_shutdown = Arc::clone(&shutdown);
        let worker_listening = Arc::clone(&listening);
        let worker_events = Arc::clone(&accepted_events);
        let worker_last_event = Arc::clone(&last_event_ms);
        let worker_error = Arc::clone(&last_error);
        let worker_path = Arc::clone(&database_path);

        let handle = thread::Builder::new()
            .name("domainflow-browser-diagnostics".to_string())
            .spawn(move || {
                worker_listening.store(true, Ordering::SeqCst);
                let mut storage_path = PathBuf::new();
                let mut storage: Option<BrowserActivityStorage> = None;

                while !worker_shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let requested_path = worker_path
                                .read()
                                .map(|value| value.clone())
                                .unwrap_or_default();
                            if requested_path != storage_path {
                                match BrowserActivityStorage::open(&requested_path) {
                                    Ok(opened) => {
                                        storage = Some(opened);
                                        storage_path = requested_path;
                                    }
                                    Err(error) => {
                                        set_last_error(&worker_error, error.to_string());
                                        let _ = write_json_response(
                                            &mut stream,
                                            500,
                                            r#"{"ok":false,"error":"database unavailable"}"#,
                                        );
                                        continue;
                                    }
                                }
                            }

                            match handle_connection(&mut stream, storage.as_mut()) {
                                Ok(accepted) => {
                                    if accepted {
                                        worker_events.fetch_add(1, Ordering::Relaxed);
                                        worker_last_event
                                            .store(now_millis().max(0) as u64, Ordering::Relaxed);
                                    }
                                }
                                Err(error) => {
                                    set_last_error(&worker_error, error.to_string());
                                    let _ = write_json_response(
                                        &mut stream,
                                        400,
                                        r#"{"ok":false,"error":"invalid event"}"#,
                                    );
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(40));
                        }
                        Err(error) => {
                            set_last_error(&worker_error, error.to_string());
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                worker_listening.store(false, Ordering::SeqCst);
            })?;

        Ok(Self {
            shutdown,
            listening,
            accepted_events,
            last_event_ms,
            last_error,
            database_path,
            handle: Some(handle),
        })
    }

    pub fn set_database_path(&self, path: PathBuf) {
        if let Ok(mut current) = self.database_path.write() {
            *current = path;
        }
    }

    pub fn status(&self) -> BrowserServerStatus {
        let last = self.last_event_ms.load(Ordering::Relaxed);
        BrowserServerStatus {
            listening: self.listening.load(Ordering::SeqCst),
            accepted_events: self.accepted_events.load(Ordering::Relaxed),
            last_event_ms: (last > 0).then_some(last),
            last_error: self.last_error.lock().ok().and_then(|value| value.clone()),
        }
    }

    pub fn shutdown(mut self) -> Result<(), BrowserActivityError> {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", BROWSER_DIAGNOSTICS_PORT));
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| BrowserActivityError::ThreadPanicked)?;
        }
        Ok(())
    }
}

impl Drop for BrowserActivityServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", BROWSER_DIAGNOSTICS_PORT));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    storage: Option<&mut BrowserActivityStorage>,
) -> Result<bool, BrowserActivityError> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = read_http_request(stream)?;

    if request.method == "OPTIONS" {
        write_empty_response(stream, 204)?;
        return Ok(false);
    }
    if request.method == "GET" && request.path == "/health" {
        write_json_response(stream, 200, r#"{"ok":true}"#)?;
        return Ok(false);
    }
    if request.method != "POST" || request.path != "/events" {
        write_json_response(stream, 404, r#"{"ok":false,"error":"not found"}"#)?;
        return Ok(false);
    }

    let payload: BrowserEventPayload = serde_json::from_slice(&request.body)?;
    let Some(storage) = storage else {
        return Err(BrowserActivityError::Protocol("database not initialized"));
    };
    storage.record(payload)?;
    write_json_response(stream, 200, r#"{"ok":true}"#)?;
    Ok(true)
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, BrowserActivityError> {
    let mut data = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(BrowserActivityError::Protocol("unexpected EOF"));
        }
        data.extend_from_slice(&buffer[..read]);
        if data.len() > MAX_HTTP_HEADER_BYTES + MAX_EVENT_BYTES {
            return Err(BrowserActivityError::Protocol("request too large"));
        }
        if let Some(index) = find_bytes(&data, b"\r\n\r\n") {
            break index + 4;
        }
        if data.len() > MAX_HTTP_HEADER_BYTES {
            return Err(BrowserActivityError::Protocol("headers too large"));
        }
    };

    let header_text = std::str::from_utf8(&data[..header_end])
        .map_err(|_| BrowserActivityError::Protocol("headers are not UTF-8"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(BrowserActivityError::Protocol("missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or(BrowserActivityError::Protocol("missing method"))?
        .to_ascii_uppercase();
    let path = request_parts
        .next()
        .ok_or(BrowserActivityError::Protocol("missing path"))?
        .to_string();

    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| BrowserActivityError::Protocol("invalid content length"))?;
        }
    }
    if content_length > MAX_EVENT_BYTES {
        return Err(BrowserActivityError::Protocol("event body too large"));
    }

    while data.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(BrowserActivityError::Protocol("truncated body"));
        }
        data.extend_from_slice(&buffer[..read]);
    }
    let body = data[header_end..header_end + content_length].to_vec();
    Ok(HttpRequest { method, path, body })
}

fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
) -> Result<(), BrowserActivityError> {
    let reason = status_reason(status);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn write_empty_response(stream: &mut TcpStream, status: u16) -> Result<(), BrowserActivityError> {
    let reason = status_reason(status);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Response",
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn normalize_required(value: String, field: &'static str) -> Result<String, BrowserActivityError> {
    let value = clamp_text(value.trim());
    if value.is_empty() {
        Err(BrowserActivityError::Protocol(field))
    } else {
        Ok(value)
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = clamp_text(value.trim());
        (!value.is_empty()).then_some(value)
    })
}

fn clamp_text(value: &str) -> String {
    if value.len() <= MAX_TEXT_BYTES {
        return value.to_string();
    }
    let mut boundary = MAX_TEXT_BYTES;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

fn nonnegative_optional(value: Option<i64>) -> Option<i64> {
    value.filter(|value| *value >= 0)
}

fn normalized_filter(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn optional_nonnegative(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn set_last_error(target: &Mutex<Option<String>>, value: String) {
    if let Ok(mut error) = target.lock() {
        *error = Some(value);
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "browser_activity_{}_{}_{}",
            std::process::id(),
            now_millis(),
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

    fn request_payload() -> BrowserEventPayload {
        BrowserEventPayload {
            kind: "request".to_string(),
            event_id: "request-1".to_string(),
            timestamp_ms: 1_000,
            host: "Example.COM".to_string(),
            url: "https://example.com/assets/model.bin".to_string(),
            final_url: None,
            page_url: Some("https://example.com/".to_string()),
            initiator: Some("https://example.com".to_string()),
            method: Some("GET".to_string()),
            resource_type: Some("fetch".to_string()),
            status_code: Some(200),
            mime: Some("application/octet-stream".to_string()),
            declared_bytes: Some(4096),
            content_disposition: None,
            error_text: None,
            filename: None,
            total_bytes: None,
            state: None,
            danger: None,
            exists_local: None,
            started_at_ms: None,
            ended_at_ms: None,
        }
    }

    #[test]
    fn request_metadata_is_persisted_and_grouped() {
        let path = temp_db_path("request");
        let mut storage = BrowserActivityStorage::open(&path).unwrap();
        storage.record(request_payload()).unwrap();

        let rows = storage
            .recent_requests(Some("example.com"), 0, 10)
            .unwrap();
        let summary = storage
            .resource_summary(Some("example.com"), 0, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host, "example.com");
        assert_eq!(rows[0].declared_bytes, Some(4096));
        assert_eq!(summary[0].resource_type, "fetch");
        assert_eq!(summary[0].declared_bytes, 4096);
        cleanup(&path);
    }

    #[test]
    fn download_updates_preserve_known_fields() {
        let path = temp_db_path("download");
        let mut storage = BrowserActivityStorage::open(&path).unwrap();
        let mut payload = request_payload();
        payload.kind = "download".to_string();
        payload.event_id = "download-7".to_string();
        payload.filename = Some(r"C:\Downloads\report.zip".to_string());
        payload.total_bytes = Some(5000);
        payload.state = Some("in_progress".to_string());
        storage.record(payload.clone()).unwrap();

        payload.timestamp_ms = 2_000;
        payload.filename = None;
        payload.total_bytes = None;
        payload.state = Some("complete".to_string());
        payload.ended_at_ms = Some(2_000);
        storage.record(payload).unwrap();

        let rows = storage
            .recent_downloads(Some("example.com"), 0, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename.as_deref(), Some(r"C:\Downloads\report.zip"));
        assert_eq!(rows[0].total_bytes, Some(5000));
        assert_eq!(rows[0].state.as_deref(), Some("complete"));
        cleanup(&path);
    }

    #[test]
    fn payload_rejects_unsupported_kind() {
        let mut payload = request_payload();
        payload.kind = "body".to_string();
        assert!(payload.normalize().is_err());
    }

    #[test]
    fn http_parser_reads_json_body() {
        let body = br#"{"kind":"request"}"#;
        let request = format!(
            "POST /events HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let parsed = read_http_request(&mut stream).unwrap();
        client.join().unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/events");
        assert_eq!(parsed.body, body);
    }
}
