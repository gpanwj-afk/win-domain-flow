use crate::app_storage::TrafficPeriod;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PRODUCT_DIR: &str = "win-domain-flow";
const DATABASE_FILE: &str = "domainflow.db";
const SETTINGS_FILE: &str = "settings.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_key(value: &str) -> Self {
        if value.eq_ignore_ascii_case("dark") {
            Self::Dark
        } else {
            Self::Light
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSettings {
    pub selected_device: Option<String>,
    pub database_path: PathBuf,
    pub period: TrafficPeriod,
    pub row_limit: u32,
    pub auto_refresh: bool,
    pub refresh_seconds: u64,
    pub theme: ThemeMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            selected_device: None,
            database_path: default_database_path(),
            period: TrafficPeriod::MonthToDate,
            row_limit: 40,
            auto_refresh: true,
            refresh_seconds: 1,
            theme: ThemeMode::Light,
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        Self::load_with_notice().0
    }

    /// Loads persisted settings and returns a one-time startup notice when an
    /// old working-directory database was migrated or had to be retained.
    pub fn load_with_notice() -> (Self, Option<String>) {
        let mut settings = Self::default();
        let mut database_was_explicit = false;

        if let Ok(content) = std::fs::read_to_string(settings_path()) {
            for line in content.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "selected_device" => {
                        settings.selected_device =
                            decode_hex(value).filter(|value| !value.is_empty());
                    }
                    "database_path" => {
                        if let Some(value) = decode_hex(value) {
                            if !value.trim().is_empty() {
                                settings.database_path = absolute_path(PathBuf::from(value));
                                database_was_explicit = true;
                            }
                        }
                    }
                    "period" => settings.period = TrafficPeriod::from_key(value),
                    "row_limit" => {
                        if let Ok(value) = value.parse::<u32>() {
                            settings.row_limit = value.clamp(10, 200);
                        }
                    }
                    "auto_refresh" => settings.auto_refresh = value == "true",
                    "refresh_seconds" => {
                        if let Ok(value) = value.parse::<u64>() {
                            settings.refresh_seconds = value.clamp(1, 30);
                        }
                    }
                    "theme" => settings.theme = ThemeMode::from_key(value),
                    _ => {}
                }
            }
        }

        let resolved = if database_was_explicit {
            resolve_persisted_database(&settings.database_path)
        } else {
            resolve_default_database()
        };
        let notice = match resolved {
            Ok((path, migration_notice)) => {
                settings.database_path = path;
                migration_notice
            }
            Err(error) => {
                if !database_was_explicit {
                    settings.database_path = default_database_path();
                }
                Some(format!("数据库路径初始化失败：{error}"))
            }
        };

        settings.database_path = absolute_path(settings.database_path);
        (settings, notice)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Store an absolute database path so subsequent launches cannot silently
        // choose a different working-directory database.
        let database_path = absolute_path(self.database_path.clone());
        let content = format!(
            "selected_device={}\ndatabase_path={}\nperiod={}\nrow_limit={}\nauto_refresh={}\nrefresh_seconds={}\ntheme={}\n",
            encode_hex(self.selected_device.as_deref().unwrap_or_default()),
            encode_hex(&database_path.to_string_lossy()),
            self.period.key(),
            self.row_limit,
            self.auto_refresh,
            self.refresh_seconds,
            self.theme.key(),
        );
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, content)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(temp, path)
    }
}

pub fn product_data_dir() -> PathBuf {
    absolute_path(
        platform_data_root()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .join(PRODUCT_DIR),
    )
}

pub fn default_database_path() -> PathBuf {
    product_data_dir().join(DATABASE_FILE)
}

pub fn settings_path() -> PathBuf {
    product_data_dir().join(SETTINGS_FILE)
}

pub fn database_parent(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(product_data_dir)
}

fn legacy_working_directory_database_path() -> PathBuf {
    absolute_path(
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(DATABASE_FILE),
    )
}

/// Resolves the default database without silently splitting data between the
/// installation folder and LOCALAPPDATA. A legacy database is copied through
/// SQLite's online backup API so committed WAL pages are included without
/// checkpointing or mutating the source database.
fn resolve_default_database() -> std::io::Result<(PathBuf, Option<String>)> {
    let persistent = default_database_path();
    let legacy = legacy_working_directory_database_path();
    resolve_legacy_database(&legacy, &persistent)
}

/// Older releases persisted their automatically selected working-directory
/// database in settings.conf. Treat that exact legacy default as migratable,
/// while preserving any genuinely custom persisted path.
fn resolve_persisted_database(path: &Path) -> std::io::Result<(PathBuf, Option<String>)> {
    let path = absolute_path(path.to_path_buf());
    let persistent = default_database_path();
    let legacy = legacy_working_directory_database_path();
    if should_migrate_persisted_database(&path, &legacy, &persistent) {
        resolve_legacy_database(&legacy, &persistent)
    } else {
        Ok((path, None))
    }
}

fn should_migrate_persisted_database(path: &Path, legacy: &Path, persistent: &Path) -> bool {
    path == legacy && legacy != persistent && legacy.exists() && !persistent.exists()
}

fn resolve_legacy_database(
    legacy: &Path,
    persistent: &Path,
) -> std::io::Result<(PathBuf, Option<String>)> {
    if persistent.exists() {
        return Ok((persistent.to_path_buf(), None));
    }
    if !legacy.exists() || legacy == persistent {
        return Ok((persistent.to_path_buf(), None));
    }

    if let Some(parent) = persistent.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match copy_sqlite_snapshot(legacy, persistent) {
        Ok(()) => Ok((
            persistent.to_path_buf(),
            Some(format!(
                "已将旧数据库复制到固定数据目录：{}。原文件仍保留在 {}。",
                persistent.display(),
                legacy.display()
            )),
        )),
        Err(error) => Ok((
            legacy.to_path_buf(),
            Some(format!(
                "旧数据库自动迁移失败（{error}）。本次明确继续使用旧数据库：{}。",
                legacy.display()
            )),
        )),
    }
}

fn copy_sqlite_snapshot(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source_connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(std::io::Error::other)?;
    let temporary = destination.with_extension(format!("migrating-{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let mut destination_connection = Connection::open(&temporary).map_err(std::io::Error::other)?;

    let backup_result = {
        let backup = Backup::new(&source_connection, &mut destination_connection)
            .map_err(std::io::Error::other)?;
        backup
            .run_to_completion(128, Duration::from_millis(10), None)
            .map_err(std::io::Error::other)
    };
    drop(destination_connection);
    drop(source_connection);

    if let Err(error) = backup_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    if destination.exists() {
        let _ = std::fs::remove_file(&temporary);
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("destination already exists: {}", destination.display()),
        ));
    }
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn platform_data_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }

    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("share"))
            })
    }
}

fn encode_hex(value: &str) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn decode_hex(value: &str) -> Option<String> {
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "domainflow_settings_{}_{}_{}",
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

    fn cleanup_database(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn hex_round_trip_preserves_chinese_and_paths() {
        let value = r"C:\用户\流量数据\domainflow.db";
        assert_eq!(decode_hex(&encode_hex(value)).as_deref(), Some(value));
    }

    #[test]
    fn invalid_hex_is_rejected() {
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[test]
    fn default_view_is_month_to_date_and_light() {
        let settings = AppSettings::default();
        assert_eq!(settings.period, TrafficPeriod::MonthToDate);
        assert_eq!(settings.theme, ThemeMode::Light);
        assert!(settings.database_path.is_absolute());
    }

    #[test]
    fn theme_keys_round_trip() {
        assert_eq!(ThemeMode::from_key("light"), ThemeMode::Light);
        assert_eq!(ThemeMode::from_key("dark"), ThemeMode::Dark);
        assert_eq!(ThemeMode::Light.toggled(), ThemeMode::Dark);
        assert_eq!(ThemeMode::Dark.toggled(), ThemeMode::Light);
    }

    #[test]
    fn online_backup_copies_committed_wal_without_checkpointing_source() {
        let source = temp_db_path("wal_source");
        let destination = temp_db_path("wal_destination");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE evidence(value INTEGER NOT NULL);
             INSERT INTO evidence(value) VALUES (8192);",
            )
            .unwrap();

        let wal_path = PathBuf::from(format!("{}-wal", source.display()));
        assert!(wal_path.exists());
        let wal_len_before = std::fs::metadata(&wal_path).unwrap().len();
        assert!(wal_len_before > 0);

        copy_sqlite_snapshot(&source, &destination).unwrap();

        let copied = Connection::open(&destination).unwrap();
        let value: i64 = copied
            .query_row("SELECT value FROM evidence", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 8192);
        assert!(wal_path.exists());
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), wal_len_before);

        drop(copied);
        drop(connection);
        cleanup_database(&source);
        cleanup_database(&destination);
    }

    #[test]
    fn persisted_legacy_default_is_migrated_but_custom_path_is_not() {
        let legacy = temp_db_path("legacy_default");
        let persistent = temp_db_path("persistent_default");
        let custom = temp_db_path("custom");
        let connection = Connection::open(&legacy).unwrap();
        connection
            .execute_batch("CREATE TABLE evidence(value INTEGER); INSERT INTO evidence VALUES (7);")
            .unwrap();
        drop(connection);

        assert!(should_migrate_persisted_database(
            &legacy,
            &legacy,
            &persistent
        ));
        assert!(!should_migrate_persisted_database(
            &custom,
            &legacy,
            &persistent
        ));

        let (resolved, notice) = resolve_legacy_database(&legacy, &persistent).unwrap();
        assert_eq!(resolved, persistent);
        assert!(notice.is_some());
        let copied = Connection::open(&resolved).unwrap();
        let value: i64 = copied
            .query_row("SELECT value FROM evidence", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 7);
        drop(copied);

        assert!(!should_migrate_persisted_database(
            &legacy,
            &legacy,
            &persistent
        ));

        cleanup_database(&legacy);
        cleanup_database(&persistent);
        cleanup_database(&custom);
    }
}
