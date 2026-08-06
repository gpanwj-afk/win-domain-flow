use crate::app_storage::TrafficPeriod;
use crate::browser_activity::{
    BrowserActivityServer, BrowserActivityStorage, BrowserDownloadRow, BrowserRequestRow,
    BrowserResourceSummaryRow, BrowserServerStatus, BROWSER_DIAGNOSTICS_PORT,
};
use crate::settings::ThemeMode;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct BrowserPalette {
    card: egui::Color32,
    inner: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    blue: egui::Color32,
    green: egui::Color32,
    amber: egui::Color32,
    red: egui::Color32,
}

impl BrowserPalette {
    fn for_theme(theme: ThemeMode) -> Self {
        match theme {
            ThemeMode::Light => Self {
                card: egui::Color32::WHITE,
                inner: egui::Color32::from_rgb(248, 250, 252),
                border: egui::Color32::from_rgb(203, 213, 225),
                text: egui::Color32::from_rgb(15, 23, 42),
                muted: egui::Color32::from_rgb(71, 85, 105),
                blue: egui::Color32::from_rgb(37, 99, 235),
                green: egui::Color32::from_rgb(4, 120, 87),
                amber: egui::Color32::from_rgb(180, 83, 9),
                red: egui::Color32::from_rgb(185, 28, 28),
            },
            ThemeMode::Dark => Self {
                card: egui::Color32::from_rgb(24, 37, 58),
                inner: egui::Color32::from_rgb(15, 25, 43),
                border: egui::Color32::from_rgb(71, 85, 105),
                text: egui::Color32::from_rgb(248, 250, 252),
                muted: egui::Color32::from_rgb(203, 213, 225),
                blue: egui::Color32::from_rgb(96, 165, 250),
                green: egui::Color32::from_rgb(52, 211, 153),
                amber: egui::Color32::from_rgb(251, 191, 36),
                red: egui::Color32::from_rgb(248, 113, 113),
            },
        }
    }
}

pub struct BrowserDiagnosticsPane {
    server: Option<BrowserActivityServer>,
    server_error: Option<String>,
    database_path: PathBuf,
    host_filter: String,
    requests: Vec<BrowserRequestRow>,
    downloads: Vec<BrowserDownloadRow>,
    summary: Vec<BrowserResourceSummaryRow>,
    last_refresh: Instant,
    query_error: Option<String>,
}

impl BrowserDiagnosticsPane {
    pub fn new(database_path: PathBuf) -> Self {
        let (server, server_error) = match BrowserActivityServer::spawn(database_path.clone()) {
            Ok(server) => (Some(server), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            server,
            server_error,
            database_path,
            host_filter: String::new(),
            requests: Vec::new(),
            downloads: Vec::new(),
            summary: Vec::new(),
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            query_error: None,
        }
    }

    pub fn set_database_path(&mut self, path: PathBuf) {
        if path == self.database_path {
            return;
        }
        self.database_path = path.clone();
        if let Some(server) = self.server.as_ref() {
            server.set_database_path(path);
        }
        self.requests.clear();
        self.downloads.clear();
        self.summary.clear();
        self.last_refresh = Instant::now() - REFRESH_INTERVAL;
    }

    pub fn refresh_if_due(&mut self, period: TrafficPeriod) {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh(period);
        }
    }

    pub fn refresh(&mut self, period: TrafficPeriod) {
        let result = (|| {
            let since_ms = BrowserActivityStorage::period_start_ms(&self.database_path, period)?;
            let storage = BrowserActivityStorage::open(&self.database_path)?;
            let filter = self.normalized_filter();
            let filter = (!filter.is_empty()).then_some(filter.as_str());
            let requests = storage.recent_requests(filter, since_ms, 120)?;
            let downloads = storage.recent_downloads(filter, since_ms, 80)?;
            let summary = storage.resource_summary(filter, since_ms, 20)?;
            Ok::<_, crate::browser_activity::BrowserActivityError>((requests, downloads, summary))
        })();

        match result {
            Ok((requests, downloads, summary)) => {
                self.requests = requests;
                self.downloads = downloads;
                self.summary = summary;
                self.query_error = None;
            }
            Err(error) => self.query_error = Some(error.to_string()),
        }
        self.last_refresh = Instant::now();
    }

    pub fn render(&mut self, ui: &mut egui::Ui, theme: ThemeMode, period: TrafficPeriod) {
        self.refresh_if_due(period);
        let palette = BrowserPalette::for_theme(theme);

        section_card(ui, palette, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.heading(
                        egui::RichText::new("浏览器活动诊断")
                            .strong()
                            .color(palette.text),
                    );
                    ui.label(
                        egui::RichText::new(
                            "回答“这个域名具体在做什么”：网页请求、资源类型、MIME、来源页面与真实下载文件。",
                        )
                        .color(palette.muted),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("打开扩展安装目录").clicked() {
                        if let Err(error) = open_extension_folder() {
                            self.query_error = Some(error);
                        }
                    }
                    if ui.button("立即刷新").clicked() {
                        self.refresh(period);
                    }
                });
            });

            ui.add_space(8.0);
            render_server_status(
                ui,
                self.server_status(),
                self.server_error.as_deref(),
                palette,
            );
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("域名筛选").color(palette.muted));
                let changed = ui
                    .add_sized(
                        [340.0, 30.0],
                        egui::TextEdit::singleline(&mut self.host_filter)
                            .hint_text("例如 tlabel.tencent.com"),
                    )
                    .changed();
                if changed {
                    self.last_refresh = Instant::now() - REFRESH_INTERVAL;
                }
                if ui.button("清除").clicked() {
                    self.host_filter.clear();
                    self.refresh(period);
                }
            });
            ui.label(
                egui::RichText::new(format!(
                    "扩展只向本机 127.0.0.1:{BROWSER_DIAGNOSTICS_PORT} 发送元数据，不上传正文。"
                ))
                .small()
                .color(palette.muted),
            );

            if let Some(error) = &self.query_error {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(error).color(palette.red));
            }
        });

        ui.add_space(12.0);
        let wide = ui.available_width() >= 980.0;
        if wide {
            ui.columns(2, |columns| {
                self.render_downloads(&mut columns[0], palette);
                self.render_resource_summary(&mut columns[1], palette);
            });
        } else {
            self.render_downloads(ui, palette);
            ui.add_space(12.0);
            self.render_resource_summary(ui, palette);
        }

        ui.add_space(12.0);
        self.render_requests(ui, palette);
    }

    fn render_downloads(&self, ui: &mut egui::Ui, palette: BrowserPalette) {
        section_card(ui, palette, |ui| {
            ui.heading(
                egui::RichText::new("真实下载文件")
                    .strong()
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(
                    "来自 Edge/Chrome 下载管理器，可看到文件名、保存位置、最终 URL、MIME 与总大小。",
                )
                .small()
                .color(palette.muted),
            );
            ui.add_space(8.0);

            if self.downloads.is_empty() {
                empty_hint(
                    ui,
                    "当前筛选范围内没有浏览器下载记录。网页视频、接口响应或缓存资源不一定属于“下载文件”。",
                    palette,
                );
                return;
            }

            egui::ScrollArea::vertical()
                .id_salt("browser-downloads")
                .max_height(360.0)
                .show(ui, |ui| {
                    for row in &self.downloads {
                        download_card(ui, row, palette);
                        ui.add_space(7.0);
                    }
                });
        });
    }

    fn render_resource_summary(&self, ui: &mut egui::Ui, palette: BrowserPalette) {
        section_card(ui, palette, |ui| {
            ui.heading(
                egui::RichText::new("网页资源用途")
                    .strong()
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(
                    "按 document、media、fetch、script、image 等请求类型归类；优先使用浏览器记录的实际编码传输字节。",
                )
                .small()
                .color(palette.muted),
            );
            ui.add_space(8.0);

            if self.summary.is_empty() {
                empty_hint(ui, "尚未收到浏览器请求元数据。", palette);
                return;
            }

            let max_bytes = self
                .summary
                .iter()
                .map(|row| row.measured_bytes)
                .max()
                .unwrap_or(1)
                .max(1);
            for row in &self.summary {
                let ratio = row.measured_bytes as f32 / max_bytes as f32;
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(resource_type_label(&row.resource_type))
                            .strong()
                            .color(palette.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format_bytes(row.measured_bytes))
                                .color(palette.blue),
                        );
                    });
                });
                ui.add(
                    egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                        .desired_width(ui.available_width())
                        .fill(palette.blue),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "{} 次请求 · {} 次大小未知（实际字节缺失时回退响应声明）",
                        format_integer(row.requests),
                        format_integer(row.unknown_size_requests)
                    ))
                    .small()
                    .color(palette.muted),
                );
                ui.add_space(6.0);
            }
        });
    }

    fn render_requests(&self, ui: &mut egui::Ui, palette: BrowserPalette) {
        section_card(ui, palette, |ui| {
            ui.heading(
                egui::RichText::new("最近网页请求")
                    .strong()
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(
                    "按实际传输字节从大到小排列 URL，再结合资源类型、MIME、协议、缓存状态与来源页面判断用途。",
                )
                .small()
                .color(palette.muted),
            );
            ui.add_space(8.0);

            if self.requests.is_empty() {
                empty_hint(
                    ui,
                    "没有匹配请求。请确认 GUI 正在运行、浏览器扩展已启用，并重新访问目标网页。",
                    palette,
                );
                return;
            }

            egui::ScrollArea::vertical()
                .id_salt("browser-requests")
                .max_height(520.0)
                .show(ui, |ui| {
                    for row in &self.requests {
                        request_card(ui, row, palette);
                        ui.add_space(7.0);
                    }
                });
        });
    }

    fn normalized_filter(&self) -> String {
        self.host_filter
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches('.')
            .to_ascii_lowercase()
    }

    fn server_status(&self) -> Option<BrowserServerStatus> {
        self.server.as_ref().map(BrowserActivityServer::status)
    }
}

fn render_server_status(
    ui: &mut egui::Ui,
    status: Option<BrowserServerStatus>,
    startup_error: Option<&str>,
    palette: BrowserPalette,
) {
    match status {
        Some(status) if status.listening => {
            ui.horizontal_wrapped(|ui| {
                status_chip(ui, "本机接收器已启动", palette.green, palette);
                status_chip(
                    ui,
                    &format!("已接收 {} 条事件", format_integer(status.accepted_events)),
                    palette.blue,
                    palette,
                );
                if let Some(last) = status.last_event_ms {
                    status_chip(
                        ui,
                        &format!("最近事件 {}", format_age(last)),
                        palette.blue,
                        palette,
                    );
                }
            });
            if let Some(error) = status.last_error {
                ui.label(egui::RichText::new(error).color(palette.amber));
            }
        }
        _ => {
            status_chip(ui, "本机接收器未启动", palette.red, palette);
            if let Some(error) = startup_error {
                ui.label(egui::RichText::new(error).color(palette.red));
            }
        }
    }
}

fn section_card<R>(
    ui: &mut egui::Ui,
    palette: BrowserPalette,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::default()
        .fill(palette.card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(11)
        .inner_margin(egui::Margin::same(15))
        .show(ui, content)
        .inner
}

fn status_chip(ui: &mut egui::Ui, text: &str, color: egui::Color32, palette: BrowserPalette) {
    egui::Frame::default()
        .fill(palette.inner)
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(14)
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().strong().color(color));
        });
}

fn download_card(ui: &mut egui::Ui, row: &BrowserDownloadRow, palette: BrowserPalette) {
    egui::Frame::default()
        .fill(palette.inner)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(9)
        .inner_margin(egui::Margin::same(11))
        .show(ui, |ui| {
            let filename = row
                .filename
                .as_deref()
                .and_then(|value| Path::new(value).file_name())
                .and_then(|value| value.to_str())
                .unwrap_or("文件名尚未确定");
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(filename).strong().color(palette.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            row.total_bytes
                                .map(format_bytes)
                                .unwrap_or_else(|| "大小未知".to_string()),
                        )
                        .strong()
                        .color(palette.green),
                    );
                });
            });
            if let Some(path) = &row.filename {
                ui.label(
                    egui::RichText::new(path)
                        .small()
                        .monospace()
                        .color(palette.muted),
                );
            }
            ui.label(
                egui::RichText::new(row.final_url.as_deref().unwrap_or(&row.url))
                    .small()
                    .color(palette.blue),
            );
            ui.horizontal_wrapped(|ui| {
                status_chip(
                    ui,
                    row.state.as_deref().unwrap_or("状态未知"),
                    state_color(row.state.as_deref(), palette),
                    palette,
                );
                if let Some(mime) = &row.mime {
                    status_chip(ui, mime, palette.blue, palette);
                }
                if let Some(danger) = &row.danger {
                    if danger != "safe" {
                        status_chip(ui, danger, palette.amber, palette);
                    }
                }
                ui.label(
                    egui::RichText::new(format_age(row.updated_at_ms.max(0) as u64))
                        .small()
                        .color(palette.muted),
                );
            });
        });
}

fn request_card(ui: &mut egui::Ui, row: &BrowserRequestRow, palette: BrowserPalette) {
    egui::Frame::default()
        .fill(palette.inner)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(9)
        .inner_margin(egui::Margin::same(11))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(url_path(&row.url))
                        .strong()
                        .color(palette.text),
                )
                .on_hover_text(&row.url);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (size_label, size_color) = if let Some(bytes) = row.transferred_bytes {
                        (format!("实际传输 {}", format_bytes(bytes)), palette.blue)
                    } else if let Some(bytes) = row.declared_bytes {
                        (format!("响应声明 {}", format_bytes(bytes)), palette.amber)
                    } else {
                        ("大小未知".to_string(), palette.muted)
                    };
                    ui.label(egui::RichText::new(size_label).strong().color(size_color));
                });
            });
            ui.horizontal_wrapped(|ui| {
                status_chip(
                    ui,
                    resource_type_label(row.resource_type.as_deref().unwrap_or("other")),
                    palette.blue,
                    palette,
                );
                if let Some(mime) = &row.mime {
                    status_chip(ui, mime, palette.green, palette);
                }
                if let Some(status) = row.status_code {
                    status_chip(
                        ui,
                        &status.to_string(),
                        if (200..400).contains(&status) {
                            palette.green
                        } else {
                            palette.red
                        },
                        palette,
                    );
                }
                if let Some(protocol) = &row.protocol {
                    status_chip(ui, protocol, palette.blue, palette);
                }
                if row.from_cache == Some(true) {
                    status_chip(ui, "来自缓存", palette.amber, palette);
                }
                if let Some(method) = &row.method {
                    status_chip(ui, method, palette.muted, palette);
                }
                ui.label(
                    egui::RichText::new(format_age(row.occurred_at_ms.max(0) as u64))
                        .small()
                        .color(palette.muted),
                );
            });
            if let Some(disposition) = &row.content_disposition {
                ui.label(
                    egui::RichText::new(format!("内容处置：{disposition}"))
                        .small()
                        .color(palette.amber),
                );
            }
            if let Some(page) = row.page_url.as_deref().or(row.initiator.as_deref()) {
                ui.label(
                    egui::RichText::new(format!("来源页面：{}", shorten(page, 110)))
                        .small()
                        .color(palette.muted),
                )
                .on_hover_text(page);
            }
            if let Some(error) = &row.error_text {
                ui.label(egui::RichText::new(error).small().color(palette.red));
            }
        });
}

fn empty_hint(ui: &mut egui::Ui, text: &str, palette: BrowserPalette) {
    ui.add_space(12.0);
    ui.label(egui::RichText::new(text).color(palette.muted));
    ui.add_space(12.0);
}

fn state_color(state: Option<&str>, palette: BrowserPalette) -> egui::Color32 {
    match state {
        Some("complete") => palette.green,
        Some("interrupted") => palette.red,
        _ => palette.amber,
    }
}

fn resource_type_label(value: &str) -> &'static str {
    match value {
        "main_frame" | "sub_frame" | "document" => "页面文档",
        "stylesheet" => "样式表",
        "script" => "脚本",
        "image" => "图片",
        "font" => "字体",
        "media" => "音视频",
        "xmlhttprequest" | "xhr" => "XHR 接口",
        "fetch" => "Fetch 接口",
        "websocket" => "WebSocket",
        "ping" => "统计上报",
        "object" => "嵌入对象",
        _ => "其他资源",
    }
}

fn url_path(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let path = without_scheme
        .split_once('/')
        .map(|(_, path)| format!("/{path}"))
        .unwrap_or_else(|| "/".to_string());
    shorten(&path, 100)
}

fn shorten(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let prefix: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{prefix}…")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

fn format_age(timestamp_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(timestamp_ms);
    let seconds = now.saturating_sub(timestamp_ms) / 1_000;
    if seconds < 10 {
        "刚刚".to_string()
    } else if seconds < 60 {
        format!("{seconds} 秒前")
    } else if seconds < 3_600 {
        format!("{} 分钟前", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3_600)
    } else {
        format!("{} 天前", seconds / 86_400)
    }
}

fn extension_dir() -> PathBuf {
    let packaged = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("browser-extension")));
    if let Some(path) = packaged.filter(|path| path.is_dir()) {
        return path;
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("browser-extension")
}

#[cfg(windows)]
fn open_extension_folder() -> Result<(), String> {
    let folder = extension_dir();
    if !folder.is_dir() {
        return Err(format!("未找到浏览器扩展目录：{}", folder.display()));
    }
    std::process::Command::new("explorer.exe")
        .arg(&folder)
        .spawn()
        .map_err(|error| format!("无法打开扩展目录：{error}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn open_extension_folder() -> Result<(), String> {
    Err(format!("扩展目录：{}", extension_dir().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_filter_accepts_full_url() {
        let mut pane = BrowserDiagnosticsPane {
            server: None,
            server_error: None,
            database_path: PathBuf::new(),
            host_filter: "https://TLabel.Tencent.com/path".to_string(),
            requests: Vec::new(),
            downloads: Vec::new(),
            summary: Vec::new(),
            last_refresh: Instant::now(),
            query_error: None,
        };
        assert_eq!(pane.normalized_filter(), "tlabel.tencent.com");
        pane.host_filter = "example.com.".to_string();
        assert_eq!(pane.normalized_filter(), "example.com");
    }

    #[test]
    fn resource_types_are_localized() {
        assert_eq!(resource_type_label("media"), "音视频");
        assert_eq!(resource_type_label("fetch"), "Fetch 接口");
        assert_eq!(resource_type_label("unknown"), "其他资源");
    }

    #[test]
    fn url_path_hides_origin_but_keeps_resource() {
        assert_eq!(
            url_path("https://example.com/assets/app.js?x=1"),
            "/assets/app.js?x=1"
        );
    }
}
