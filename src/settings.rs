use crate::app_storage::TrafficPeriod;
use std::path::{Path, PathBuf};

const PRODUCT_DIR: &str = "win-domain-flow";
const DATABASE_FILE: &str = "domainflow.db";
const SETTINGS_FILE: &str = "settings.conf";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSettings {
    pub selected_device: Option<String>,
    pub database_path: PathBuf,
    pub period: TrafficPeriod,
    pub row_limit: u32,
    pub auto_refresh: bool,
    pub refresh_seconds: u64,
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
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        let mut settings = Self::default();
        let Ok(content) = std::fs::read_to_string(settings_path()) else {
            return settings;
        };

        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "selected_device" => {
                    settings.selected_device = decode_hex(value).filter(|value| !value.is_empty());
                }
                "database_path" => {
                    if let Some(value) = decode_hex(value) {
                        if !value.trim().is_empty() {
                            settings.database_path = PathBuf::from(value);
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
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = format!(
            "selected_device={}\ndatabase_path={}\nperiod={}\nrow_limit={}\nauto_refresh={}\nrefresh_seconds={}\n",
            encode_hex(self.selected_device.as_deref().unwrap_or_default()),
            encode_hex(&self.database_path.to_string_lossy()),
            self.period.key(),
            self.row_limit,
            self.auto_refresh,
            self.refresh_seconds,
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
    platform_data_root()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(PRODUCT_DIR)
}

pub fn default_database_path() -> PathBuf {
    let persistent = product_data_dir().join(DATABASE_FILE);
    if persistent.exists() {
        return persistent;
    }

    let legacy = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(DATABASE_FILE);
    if legacy.exists() {
        legacy
    } else {
        persistent
    }
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

fn platform_data_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
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
    fn default_view_is_month_to_date() {
        assert_eq!(AppSettings::default().period, TrafficPeriod::MonthToDate);
    }
}
