use crate::app_runtime::{run_live_with_shutdown, ApplicationRunSummary};
use crate::app_storage::{ApplicationStorage, TrafficPeriod};
use crate::browser_ui::BrowserDiagnosticsPane;
use crate::capture::{list_devices, CaptureDeviceInfo};
use crate::model::{
    TopApplicationRow, TopDomainDetailRow, TrafficTotals, HISTORICAL_APPLICATION,
    UNKNOWN_APPLICATION, UNKNOWN_DOMAIN,
};
use crate::runtime::RuntimeConfig;
use crate::settings::{database_parent, product_data_dir, AppSettings, ThemeMode};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const APP_TITLE: &str = "域流量管家";
const UI_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy)]
struct Palette {
    bg: egui::Color32,
    panel: egui::Color32,
    card: egui::Color32,
    card_hover: egui::Color32,
    input: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    blue: egui::Color32,
    cyan: egui::Color32,
    green: egui::Color32,
    amber: egui::Color32,
    red: egui::Color32,
    on_accent: egui::Color32,
    on_warning: egui::Color32,
}

impl Palette {
    fn for_theme(theme: ThemeMode) -> Self {
        match theme {
            ThemeMode::Light => Self {
                bg: egui::Color32::from_rgb(244, 247, 251),
                panel: egui::Color32::from_rgb(255, 255, 255),
                card: egui::Color32::from_rgb(255, 255, 255),
                card_hover: egui::Color32::from_rgb(237, 244, 255),
                input: egui::Color32::from_rgb(248, 250, 252),
                border: egui::Color32::from_rgb(203, 213, 225),
                text: egui::Color32::from_rgb(15, 23, 42),
                muted: egui::Color32::from_rgb(71, 85, 105),
                blue: egui::Color32::from_rgb(37, 99, 235),
                cyan: egui::Color32::from_rgb(8, 145, 178),
                green: egui::Color32::from_rgb(4, 120, 87),
                amber: egui::Color32::from_rgb(180, 83, 9),
                red: egui::Color32::from_rgb(185, 28, 28),
                on_accent: egui::Color32::WHITE,
                on_warning: egui::Color32::WHITE,
            },
            ThemeMode::Dark => Self {
                bg: egui::Color32::from_rgb(11, 18, 32),
                panel: egui::Color32::from_rgb(17, 27, 46),
                card: egui::Color32::from_rgb(24, 37, 58),
                card_hover: egui::Color32::from_rgb(34, 51, 77),
                input: egui::Color32::from_rgb(15, 25, 43),
                border: egui::Color32::from_rgb(71, 85, 105),
                text: egui::Color32::from_rgb(248, 250, 252),
                muted: egui::Color32::from_rgb(203, 213, 225),
                blue: egui::Color32::from_rgb(96, 165, 250),
                cyan: egui::Color32::from_rgb(34, 211, 238),
                green: egui::Color32::from_rgb(52, 211, 153),
                amber: egui::Color32::from_rgb(251, 191, 36),
                red: egui::Color32::from_rgb(248, 113, 113),
                on_accent: egui::Color32::from_rgb(8, 18, 35),
                on_warning: egui::Color32::from_rgb(40, 24, 3),
            },
        }
    }
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 680.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|creation_context| Ok(Box::new(DashboardApp::new(creation_context)))),
    )
}

#[derive(Debug, Clone)]
enum CaptureState {
    Idle,
    Running,
    Stopping,
    Finished,
    Failed(String),
}

struct CaptureWorker {
    shutdown: Arc<AtomicBool>,
    result_rx: Receiver<Result<ApplicationRunSummary, String>>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    fn request_stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn try_result(&self) -> Option<Result<ApplicationRunSummary, String>> {
        match self.result_rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("抓包线程意外断开，请重新启动程序。".to_string()))
            }
        }
    }

    fn join(&mut self) -> Result<(), String> {
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| "抓包线程发生异常。".to_string())?;
        }
        Ok(())
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct DashboardApp {
    devices: Vec<CaptureDeviceInfo>,
    selected_device: Option<String>,
    database_path: String,
    period: TrafficPeriod,
    row_limit: u32,
    auto_refresh: bool,
    refresh_seconds: u64,
    theme: ThemeMode,
    application_search: String,
    applications: Vec<TopApplicationRow>,
    selected_application: Option<String>,
    domains: Vec<TopDomainDetailRow>,
    totals: TrafficTotals,
    state: CaptureState,
    capture: Option<CaptureWorker>,
    last_summary: Option<ApplicationRunSummary>,
    last_refresh: Instant,
    last_rate_sample: Instant,
    last_total_bytes: u64,
    bytes_per_second: f64,
    notice: Option<String>,
    browser_diagnostics: BrowserDiagnosticsPane,
}

impl DashboardApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        install_chinese_font(&creation_context.egui_ctx);
        let settings = AppSettings::load();
        configure_style(&creation_context.egui_ctx, settings.theme);

        let browser_database_path = settings.database_path.clone();
        let now = Instant::now();
        let mut app = Self {
            devices: Vec::new(),
            selected_device: settings.selected_device,
            database_path: settings.database_path.to_string_lossy().to_string(),
            period: settings.period,
            row_limit: settings.row_limit,
            auto_refresh: settings.auto_refresh,
            refresh_seconds: settings.refresh_seconds,
            theme: settings.theme,
            application_search: String::new(),
            applications: Vec::new(),
            selected_application: None,
            domains: Vec::new(),
            totals: TrafficTotals::default(),
            state: CaptureState::Idle,
            capture: None,
            last_summary: None,
            last_refresh: now,
            last_rate_sample: now,
            last_total_bytes: 0,
            bytes_per_second: 0.0,
            notice: None,
            browser_diagnostics: BrowserDiagnosticsPane::new(browser_database_path),
        };
        app.refresh_devices();
        app.refresh_data(false);
        app
    }

    fn palette(&self) -> Palette {
        Palette::for_theme(self.theme)
    }

    fn settings(&self) -> AppSettings {
        AppSettings {
            selected_device: self.selected_device.clone(),
            database_path: PathBuf::from(self.database_path.trim()),
            period: self.period,
            row_limit: self.row_limit,
            auto_refresh: self.auto_refresh,
            refresh_seconds: self.refresh_seconds,
            theme: self.theme,
        }
    }

    fn save_settings(&self) {
        let _ = self.settings().save();
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.theme = self.theme.toggled();
        configure_style(ctx, self.theme);
        self.save_settings();
    }

    fn refresh_devices(&mut self) {
        let previous = self.selected_device.clone();
        match list_devices() {
            Ok(devices) => {
                self.devices = devices;
                self.selected_device = previous
                    .filter(|name| self.devices.iter().any(|device| &device.name == name))
                    .or_else(|| choose_preferred_device(&self.devices));
                self.notice = None;
                self.save_settings();
            }
            Err(error) => {
                self.devices.clear();
                self.selected_device = None;
                self.notice = Some(format!("无法读取 Npcap 网卡：{error}"));
            }
        }
    }

    fn start_capture(&mut self) {
        if self.capture.is_some() {
            return;
        }

        let Some(interface) = self.selected_device.clone() else {
            self.notice = Some("请先选择用于联网的物理网卡。".to_string());
            return;
        };
        let database_text = self.database_path.trim();
        if database_text.is_empty() {
            self.notice = Some("数据库路径不能为空。".to_string());
            return;
        }

        let database_path = PathBuf::from(database_text);
        if let Some(parent) = database_path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                self.notice = Some(format!("无法创建数据目录：{error}"));
                return;
            }
        }

        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (result_tx, result_rx) = mpsc::channel();
        let config = RuntimeConfig::default();
        let spawn_result = thread::Builder::new()
            .name("domainflow-gui-capture".to_string())
            .spawn(move || {
                let result = run_live_with_shutdown(
                    &interface,
                    &database_path,
                    config,
                    Arc::clone(&worker_shutdown),
                )
                .map_err(|error| error.to_string());
                let _ = result_tx.send(result);
            });

        match spawn_result {
            Ok(handle) => {
                self.capture = Some(CaptureWorker {
                    shutdown,
                    result_rx,
                    handle: Some(handle),
                });
                self.state = CaptureState::Running;
                self.last_summary = None;
                self.notice = Some("正在记录流量，数据会持续写入 SQLite。".to_string());
                self.refresh_data(false);
                self.last_total_bytes = self.totals.bytes;
                self.last_rate_sample = Instant::now();
                self.bytes_per_second = 0.0;
                self.save_settings();
            }
            Err(error) => {
                self.state = CaptureState::Failed(error.to_string());
                self.notice = Some(format!("无法启动抓包线程：{error}"));
            }
        }
    }

    fn stop_capture(&mut self) {
        if let Some(worker) = self.capture.as_ref() {
            worker.request_stop();
            self.state = CaptureState::Stopping;
            self.notice = Some("正在停止抓包并安全写入最后一批数据……".to_string());
        }
    }

    fn poll_capture(&mut self) {
        let result = self.capture.as_ref().and_then(CaptureWorker::try_result);
        let Some(mut result) = result else {
            return;
        };

        if let Some(mut worker) = self.capture.take() {
            if let Err(join_error) = worker.join() {
                result = Err(join_error);
            }
        }

        match result {
            Ok(summary) => {
                self.last_summary = Some(summary);
                self.state = CaptureState::Finished;
                self.notice = Some("抓包已停止，待处理流量已全部保存。".to_string());
            }
            Err(error) => {
                self.state = CaptureState::Failed(error.clone());
                self.notice = Some(error);
            }
        }
        self.refresh_data(true);
    }

    fn refresh_data(&mut self, update_rate: bool) {
        let path = Path::new(self.database_path.trim());
        if self.database_path.trim().is_empty() || !path.is_file() {
            self.applications.clear();
            self.domains.clear();
            self.totals = TrafficTotals::default();
            self.last_refresh = Instant::now();
            if update_rate {
                self.bytes_per_second = 0.0;
            }
            return;
        }

        let result = ApplicationStorage::open(path).and_then(|storage| {
            let applications = storage.top_applications(self.period, self.row_limit)?;
            let selected = self
                .selected_application
                .as_deref()
                .filter(|selected| applications.iter().any(|row| row.application == *selected));
            let domains = storage.top_domain_details(self.period, selected, self.row_limit)?;
            let totals = storage.totals(self.period)?;
            Ok((applications, selected.map(str::to_string), domains, totals))
        });

        match result {
            Ok((applications, selected, domains, totals)) => {
                self.applications = applications;
                self.selected_application = selected;
                self.domains = domains;
                self.totals = totals;
                if !matches!(self.state, CaptureState::Failed(_)) {
                    self.notice = None;
                }
                if update_rate {
                    self.update_rate();
                }
            }
            Err(error) => {
                self.notice = Some(format!("读取流量数据库失败：{error}"));
            }
        }
        self.last_refresh = Instant::now();
    }

    fn update_rate(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_rate_sample).as_secs_f64();
        if elapsed > 0.0 {
            let growth = self.totals.bytes.saturating_sub(self.last_total_bytes);
            self.bytes_per_second = growth as f64 / elapsed;
        }
        self.last_total_bytes = self.totals.bytes;
        self.last_rate_sample = now;
    }

    fn select_application(&mut self, application: Option<String>) {
        self.selected_application = application;
        self.refresh_data(false);
    }

    fn open_database_folder(&mut self) {
        let path = PathBuf::from(self.database_path.trim());
        let folder = database_parent(&path);
        if let Err(error) = std::fs::create_dir_all(&folder) {
            self.notice = Some(format!("无法打开数据目录：{error}"));
            return;
        }

        #[cfg(windows)]
        {
            if let Err(error) = std::process::Command::new("explorer.exe")
                .arg(&folder)
                .spawn()
            {
                self.notice = Some(format!("无法打开数据目录：{error}"));
            }
        }

        #[cfg(not(windows))]
        {
            self.notice = Some(format!("数据目录：{}", folder.display()));
        }
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui, palette: Palette) {
        section_title(ui, "抓包控制", "选择当前联网网卡后即可持续记录", palette);
        card(ui, palette, |ui| {
            ui.label(egui::RichText::new("联网网卡").color(palette.muted));
            let selected_text = self
                .selected_device
                .as_ref()
                .and_then(|name| self.devices.iter().find(|device| &device.name == name))
                .map(device_short_label)
                .unwrap_or_else(|| "请选择网卡".to_string());

            let enabled = self.capture.is_none();
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("capture-adapter")
                    .selected_text(egui::RichText::new(selected_text).color(palette.text))
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for device in &self.devices {
                            ui.selectable_value(
                                &mut self.selected_device,
                                Some(device.name.clone()),
                                device_short_label(device),
                            )
                            .on_hover_text(device_label(device));
                        }
                    });
            });
            if ui
                .add_enabled(enabled, egui::Button::new("↻ 重新扫描网卡"))
                .clicked()
            {
                self.refresh_devices();
            }

            ui.add_space(8.0);
            let capture_button = if self.capture.is_none() {
                egui::Button::new(
                    egui::RichText::new("▶ 开始记录流量")
                        .strong()
                        .color(palette.on_accent),
                )
                .fill(palette.blue)
            } else {
                egui::Button::new(
                    egui::RichText::new("■ 停止并安全保存")
                        .strong()
                        .color(palette.on_warning),
                )
                .fill(palette.amber)
            };
            let clicked = ui
                .add_sized([ui.available_width(), 44.0], capture_button)
                .clicked();
            if clicked {
                if self.capture.is_none() {
                    self.start_capture();
                } else {
                    self.stop_capture();
                }
            }
            ui.add_space(7.0);
            status_badge(ui, &self.state, palette);
            ui.label(
                egui::RichText::new("Npcap 抓包通常需要以管理员身份运行。")
                    .small()
                    .color(palette.muted),
            );
        });

        ui.add_space(14.0);
        section_title(
            ui,
            "数据保存",
            "关闭或重启不会清空，本月数据持续累加",
            palette,
        );
        card(ui, palette, |ui| {
            ui.label(egui::RichText::new("SQLite 数据库").color(palette.muted));
            ui.add_enabled(
                self.capture.is_none(),
                egui::TextEdit::singleline(&mut self.database_path)
                    .desired_width(ui.available_width()),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button("打开数据目录").clicked() {
                    self.open_database_folder();
                }
                if ui
                    .add_enabled(self.capture.is_none(), egui::Button::new("使用默认目录"))
                    .clicked()
                {
                    self.database_path = product_data_dir()
                        .join("domainflow.db")
                        .to_string_lossy()
                        .to_string();
                    self.refresh_data(false);
                    self.save_settings();
                }
            });
            ui.label(
                egui::RichText::new("✓ 数据按天持久化；默认显示月初至今。")
                    .small()
                    .color(palette.green),
            );
        });

        ui.add_space(14.0);
        section_title(ui, "显示设置", "主题、统计周期与刷新频率", palette);
        card(ui, palette, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("当前主题").color(palette.muted));
                ui.label(
                    egui::RichText::new(theme_label(self.theme))
                        .strong()
                        .color(palette.text),
                );
            });
            ui.label(
                egui::RichText::new("可在窗口右上角随时切换。")
                    .small()
                    .color(palette.muted),
            );
            ui.separator();
            ui.label(egui::RichText::new("统计周期").color(palette.muted));
            period_selector(ui, &mut self.period);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("最多显示");
                if ui
                    .add(egui::DragValue::new(&mut self.row_limit).range(10..=200))
                    .changed()
                {
                    self.refresh_data(false);
                }
                ui.label("条");
            });
            ui.checkbox(&mut self.auto_refresh, "自动刷新");
            ui.horizontal(|ui| {
                ui.label("每");
                ui.add(egui::DragValue::new(&mut self.refresh_seconds).range(1..=30));
                ui.label("秒");
            });
            if ui.button("立即刷新").clicked() {
                self.refresh_data(true);
            }
        });

        if let Some(summary) = &self.last_summary {
            ui.add_space(14.0);
            section_title(ui, "最近一次抓包", "安全停止后的运行摘要", palette);
            card(ui, palette, |ui| {
                summary_row(ui, "捕获数据包", summary.captured_packets, palette);
                summary_row(ui, "纳入统计", summary.accepted_packets, palette);
                summary_row(ui, "应用已归因", summary.attributed_packets, palette);
                summary_row(ui, "应用未归因", summary.attribution_misses, palette);
                summary_row(ui, "解析错误", summary.parse_errors, palette);
                summary_row(ui, "写入批次", summary.submitted_batches, palette);
            });
        }

        if let Some(notice) = &self.notice {
            ui.add_space(12.0);
            let color = if matches!(self.state, CaptureState::Failed(_)) {
                palette.red
            } else {
                palette.muted
            };
            card(ui, palette, |ui| {
                ui.label(egui::RichText::new(notice).color(color));
            });
        }
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui, palette: Palette) {
        self.browser_diagnostics
            .set_database_path(PathBuf::from(self.database_path.trim()));

        let app_coverage = percentage(
            self.totals
                .bytes
                .saturating_sub(self.totals.unknown_application_bytes),
            self.totals.bytes,
        );
        let domain_coverage = percentage(
            self.totals
                .bytes
                .saturating_sub(self.totals.unknown_domain_bytes),
            self.totals.bytes,
        );

        let available_width = ui.available_width();
        let metric_columns = responsive_metric_columns(available_width);
        let gap = 10.0;
        let metric_width = ((available_width - gap * (metric_columns as f32 - 1.0))
            / metric_columns as f32)
            .max(145.0);
        let metrics = [
            ("本期总流量", format_bytes(self.totals.bytes), palette.blue),
            ("数据包", format_integer(self.totals.packets), palette.cyan),
            ("应用归因率", format!("{app_coverage:.1}%"), palette.green),
            (
                "域名识别率",
                format!("{domain_coverage:.1}%"),
                palette.amber,
            ),
            ("实时写入", format_rate(self.bytes_per_second), palette.cyan),
        ];
        egui::Grid::new("responsive-metrics")
            .num_columns(metric_columns)
            .spacing([gap, gap])
            .show(ui, |ui| {
                for (index, (title, value, color)) in metrics.iter().enumerate() {
                    metric_card(ui, title, value, *color, palette, metric_width);
                    if (index + 1) % metric_columns == 0 {
                        ui.end_row();
                    }
                }
                if metrics.len() % metric_columns != 0 {
                    ui.end_row();
                }
            });

        ui.add_space(14.0);
        card(ui, palette, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.heading(
                        egui::RichText::new(period_title(self.period))
                            .strong()
                            .color(palette.text),
                    );
                    ui.label(
                        egui::RichText::new("持久化 SQLite · 关闭程序后数据仍保留")
                            .small()
                            .color(palette.muted),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let search_width = ui.available_width().clamp(180.0, 340.0);
                    ui.add_sized(
                        [search_width, 32.0],
                        egui::TextEdit::singleline(&mut self.application_search)
                            .hint_text("搜索应用，例如 edge、微信"),
                    );
                });
            });
        });

        ui.add_space(12.0);
        render_visibility_boundary(ui, palette);
        ui.add_space(12.0);

        let content_width = ui.available_width();
        if content_width >= 980.0 {
            ui.columns(2, |columns| {
                self.render_applications(&mut columns[0], palette);
                self.render_domains(&mut columns[1], palette);
            });
        } else {
            self.render_applications(ui, palette);
            ui.add_space(12.0);
            self.render_domains(ui, palette);
        }

        ui.add_space(14.0);
        self.browser_diagnostics.render(ui, self.theme, self.period);
    }

    fn render_applications(&mut self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            ui.horizontal(|ui| {
                ui.heading(
                    egui::RichText::new("应用流量排行")
                        .strong()
                        .color(palette.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} 个应用", self.applications.len()))
                            .color(palette.muted),
                    );
                });
            });
            ui.label(
                egui::RichText::new("选择应用后，右侧显示该应用访问的域名；具体网页用途请继续查看下方浏览器活动诊断。")
                    .small()
                    .color(palette.muted),
            );
            ui.add_space(8.0);

            let all_selected = self.selected_application.is_none();
            if application_row(
                ui,
                "全部应用",
                self.totals.bytes,
                self.totals.packets,
                1.0,
                all_selected,
                palette.blue,
                palette,
            ) {
                self.select_application(None);
            }

            let query = self.application_search.trim().to_lowercase();
            let rows: Vec<TopApplicationRow> = self
                .applications
                .iter()
                .filter(|row| {
                    query.is_empty()
                        || display_application(&row.application)
                            .to_lowercase()
                            .contains(&query)
                })
                .cloned()
                .collect();
            let max_bytes = rows.first().map_or(1, |row| row.bytes.max(1));
            let mut pending = None;
            egui::ScrollArea::vertical()
                .id_salt("applications-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in rows {
                        let selected =
                            self.selected_application.as_deref() == Some(row.application.as_str());
                        let color = if row.application == UNKNOWN_APPLICATION {
                            palette.amber
                        } else if row.application == HISTORICAL_APPLICATION {
                            palette.muted
                        } else {
                            palette.blue
                        };
                        if application_row(
                            ui,
                            &display_application(&row.application),
                            row.bytes,
                            row.packets,
                            row.bytes as f32 / max_bytes as f32,
                            selected,
                            color,
                            palette,
                        ) {
                            pending = Some(row.application);
                        }
                    }
                });
            if let Some(application) = pending {
                self.select_application(Some(application));
            }
        });
    }

    fn render_domains(&self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            let app_title = self
                .selected_application
                .as_deref()
                .map(display_application)
                .unwrap_or_else(|| "全部应用".to_string());
            ui.heading(
                egui::RichText::new(format!("{app_title} · 访问域名"))
                    .strong()
                    .color(palette.text),
            );
            ui.label(
            egui::RichText::new(
                "这里保留域名总量与数据包排行。要判断大流量具体用于下载、视频、接口或脚本，请在下方输入该域名。",
            )
            .small()
            .color(palette.muted),
        );
            ui.add_space(10.0);

            if self.domains.is_empty() {
                empty_state(ui, palette);
                return;
            }

            let max_bytes = self.domains.first().map_or(1, |row| row.bytes.max(1));
            egui::ScrollArea::vertical()
                .id_salt("domains-scroll")
                .max_height(520.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in &self.domains {
                        domain_total_card(ui, row, max_bytes, palette);
                        ui.add_space(8.0);
                    }
                });
        });
    }
}

impl Drop for DashboardApp {
    fn drop(&mut self) {
        if let Some(worker) = self.capture.as_ref() {
            worker.request_stop();
        }
        self.save_settings();
    }
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_capture();
        if self.auto_refresh
            && self.last_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds.max(1))
        {
            self.refresh_data(true);
        }

        let palette = self.palette();
        let mut toggle_theme = false;
        egui::TopBottomPanel::top("header")
            .exact_height(78.0)
            .frame(
                egui::Frame::default()
                    .fill(palette.panel)
                    .inner_margin(egui::Margin::symmetric(22, 13))
                    .stroke(egui::Stroke::new(1.0, palette.border)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.heading(
                            egui::RichText::new("域流量管家")
                                .size(25.0)
                                .strong()
                                .color(palette.text),
                        );
                        ui.label(
                            egui::RichText::new("Windows 应用、域名与浏览器活动诊断仪表盘")
                                .color(palette.muted),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        status_badge(ui, &self.state, palette);
                        let label = match self.theme {
                            ThemeMode::Light => "🌙 深色模式",
                            ThemeMode::Dark => "☀ 明亮模式",
                        };
                        if ui.button(label).clicked() {
                            toggle_theme = true;
                        }
                    });
                });
            });
        if toggle_theme {
            self.toggle_theme(ctx);
        }

        let palette = self.palette();
        egui::SidePanel::left("controls")
            .resizable(true)
            .default_width(330.0)
            .min_width(280.0)
            .max_width(420.0)
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.render_sidebar(ui, palette);
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                let viewport_width = ui.available_width();
                egui::ScrollArea::vertical()
                    .id_salt("main-dashboard-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(viewport_width);
                        ui.set_max_width(viewport_width);
                        self.render_dashboard(ui, palette);
                    });
            });

        ctx.request_repaint_after(UI_TICK);
    }
}

fn configure_style(ctx: &egui::Context, theme: ThemeMode) {
    let palette = Palette::for_theme(theme);
    let mut style = (*ctx.style()).clone();
    style.visuals = match theme {
        ThemeMode::Light => egui::Visuals::light(),
        ThemeMode::Dark => egui::Visuals::dark(),
    };
    style.visuals.panel_fill = palette.bg;
    style.visuals.window_fill = palette.panel;
    style.visuals.extreme_bg_color = palette.input;
    style.visuals.faint_bg_color = palette.card_hover;
    style.visuals.widgets.inactive.bg_fill = palette.input;
    style.visuals.widgets.hovered.bg_fill = palette.card_hover;
    style.visuals.widgets.active.bg_fill = palette.blue;
    style.visuals.widgets.inactive.fg_stroke.color = palette.text;
    style.visuals.widgets.hovered.fg_stroke.color = palette.text;
    style.visuals.widgets.active.fg_stroke.color = palette.on_accent;
    style.visuals.override_text_color = Some(palette.text);
    style.visuals.selection.bg_fill = palette.blue;
    style.visuals.selection.stroke.color = palette.on_accent;
    style.visuals.window_stroke = egui::Stroke::new(1.0, palette.border);
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style(style);
}

fn install_chinese_font(ctx: &egui::Context) {
    let mut candidates = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        let fonts = PathBuf::from(windir).join("Fonts");
        candidates.push(fonts.join("msyh.ttc"));
        candidates.push(fonts.join("msyhbd.ttc"));
        candidates.push(fonts.join("simhei.ttf"));
    }
    candidates.push(PathBuf::from(
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ));
    candidates.push(PathBuf::from(
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ));

    let Some(bytes) = candidates
        .into_iter()
        .find_map(|path| std::fs::read(path).ok())
    else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "domainflow-cjk".to_string(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(entries) = fonts.families.get_mut(&family) {
            entries.insert(0, "domainflow-cjk".to_string());
        }
    }
    ctx.set_fonts(fonts);
}

fn card<R>(ui: &mut egui::Ui, palette: Palette, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(palette.card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(11)
        .inner_margin(egui::Margin::same(15))
        .show(ui, content)
        .inner
}

fn section_title(ui: &mut egui::Ui, title: &str, subtitle: &str, palette: Palette) {
    ui.heading(
        egui::RichText::new(title)
            .size(18.0)
            .strong()
            .color(palette.text),
    );
    ui.label(egui::RichText::new(subtitle).small().color(palette.muted));
    ui.add_space(6.0);
}

fn metric_card(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    accent: egui::Color32,
    palette: Palette,
    width: f32,
) {
    egui::Frame::default()
        .fill(palette.card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(11)
        .inner_margin(egui::Margin::symmetric(15, 12))
        .show(ui, |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.label(egui::RichText::new(title).small().color(palette.muted));
            ui.label(egui::RichText::new(value).size(22.0).strong().color(accent));
        });
}

fn responsive_metric_columns(width: f32) -> usize {
    if width >= 1180.0 {
        5
    } else if width >= 760.0 {
        3
    } else {
        2
    }
}

fn period_selector(ui: &mut egui::Ui, period: &mut TrafficPeriod) {
    ui.horizontal_wrapped(|ui| {
        for option in TrafficPeriod::ALL {
            ui.selectable_value(period, option, period_label(option));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn application_row(
    ui: &mut egui::Ui,
    name: &str,
    bytes: u64,
    packets: u64,
    ratio: f32,
    selected: bool,
    color: egui::Color32,
    palette: Palette,
) -> bool {
    let fill = if selected {
        palette.card_hover
    } else {
        palette.card
    };
    let response = egui::Frame::default()
        .fill(fill)
        .stroke(egui::Stroke::new(
            if selected { 1.5 } else { 0.7 },
            if selected { color } else { palette.border },
        ))
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(11, 9))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(shorten_text(name, 34))
                            .strong()
                            .color(palette.text),
                    );
                    ui.label(
                        egui::RichText::new(format!("{} 个数据包", format_integer(packets)))
                            .small()
                            .color(palette.muted),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format_bytes(bytes))
                            .strong()
                            .color(color),
                    );
                });
            });
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .desired_width(ui.available_width())
                    .fill(color),
            );
        })
        .response;
    response.interact(egui::Sense::click()).clicked()
}

fn domain_total_card(
    ui: &mut egui::Ui,
    row: &TopDomainDetailRow,
    max_bytes: u64,
    palette: Palette,
) {
    let accent = if row.domain == UNKNOWN_DOMAIN {
        palette.amber
    } else {
        palette.blue
    };
    egui::Frame::default()
        .fill(palette.input)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(13, 11))
        .show(ui, |ui| {
            let label = display_domain(&row.domain);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(shorten_text(&label, 48))
                        .strong()
                        .color(palette.text),
                )
                .on_hover_text(&label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format_bytes(row.bytes))
                            .strong()
                            .color(accent),
                    );
                });
            });
            let ratio = row.bytes as f32 / max_bytes.max(1) as f32;
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .desired_width(ui.available_width())
                    .fill(accent),
            );
            ui.label(
                egui::RichText::new(format!("{} 个数据包", format_integer(row.packets)))
                    .small()
                    .color(palette.muted),
            );
        });
}

fn render_visibility_boundary(ui: &mut egui::Ui, palette: Palette) {
    card(ui, palette, |ui| {
        ui.label(
            egui::RichText::new("从“流量多少”继续追到“网页在做什么”")
                .strong()
                .color(palette.text),
        );
        ui.label(
            egui::RichText::new(
                "域名总量本身无法说明业务用途。v0.5 通过可选浏览器扩展补充每个请求的实际传输字节、URL、资源类型、MIME、来源页面，以及真实下载文件名和大小。",
            )
            .color(palette.blue),
        );
        ui.separator();
        ui.label(
            egui::RichText::new(
                "HTTPS 正文仍保持加密；工具不安装中间人证书、不读取消息正文或文件内容。",
            )
            .color(palette.muted),
        );
    });
}

fn empty_state(ui: &mut egui::Ui, palette: Palette) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new("暂无流量数据")
                .size(20.0)
                .strong()
                .color(palette.text),
        );
        ui.label(
            egui::RichText::new("开始抓包并访问网页后，应用与域名会自动出现在这里。")
                .color(palette.muted),
        );
    });
}

fn status_badge(ui: &mut egui::Ui, state: &CaptureState, palette: Palette) {
    let (text, color) = match state {
        CaptureState::Idle => ("未开始", palette.muted),
        CaptureState::Running => ("● 正在抓包", palette.green),
        CaptureState::Stopping => ("● 正在安全保存", palette.amber),
        CaptureState::Finished => ("已停止并保存", palette.cyan),
        CaptureState::Failed(_) => ("发生错误", palette.red),
    };
    let response = egui::Frame::default()
        .fill(color.gamma_multiply(0.14))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(16)
        .inner_margin(egui::Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).strong().color(color))
        })
        .response;
    if let CaptureState::Failed(error) = state {
        response.on_hover_text(error);
    }
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: u64, palette: Palette) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(palette.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format_integer(value))
                    .monospace()
                    .color(palette.text),
            );
        });
    });
}

fn choose_preferred_device(devices: &[CaptureDeviceInfo]) -> Option<String> {
    devices
        .iter()
        .max_by_key(|device| device_score(device))
        .map(|device| device.name.clone())
}

fn device_score(device: &CaptureDeviceInfo) -> i32 {
    let text = format!(
        "{} {}",
        device.name,
        device.description.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    if text.contains("loopback") {
        -100
    } else if text.contains("wi-fi direct")
        || text.contains("hyper-v")
        || text.contains("virtual")
        || text.contains("tailscale")
        || text.contains("vmware")
        || text.contains("virtualbox")
    {
        -10
    } else {
        100
    }
}

fn device_short_label(device: &CaptureDeviceInfo) -> String {
    device
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
        .map(str::trim)
        .unwrap_or(&device.name)
        .to_string()
}

fn device_label(device: &CaptureDeviceInfo) -> String {
    format!("{}\n{}", device_short_label(device), device.name)
}

fn display_application(application: &str) -> String {
    match application {
        UNKNOWN_APPLICATION => "未知应用".to_string(),
        HISTORICAL_APPLICATION => "历史数据（升级前未记录应用）".to_string(),
        _ => application.to_string(),
    }
}

fn display_domain(domain: &str) -> String {
    if domain == UNKNOWN_DOMAIN {
        "未知域名".to_string()
    } else {
        domain.to_string()
    }
}

fn theme_label(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::Light => "明亮模式",
        ThemeMode::Dark => "深色模式",
    }
}

fn period_label(period: TrafficPeriod) -> &'static str {
    match period {
        TrafficPeriod::Today => "今日",
        TrafficPeriod::MonthToDate => "本月",
        TrafficPeriod::Last7Days => "近 7 天",
        TrafficPeriod::Last30Days => "近 30 天",
        TrafficPeriod::All => "全部",
    }
}

fn period_title(period: TrafficPeriod) -> &'static str {
    match period {
        TrafficPeriod::Today => "今日流量概览",
        TrafficPeriod::MonthToDate => "本月累计流量",
        TrafficPeriod::Last7Days => "近 7 天流量",
        TrafficPeriod::Last30Days => "近 30 天流量",
        TrafficPeriod::All => "全部历史流量",
    }
}

fn percentage(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn shorten_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let prefix: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{prefix}…")
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/秒", format_bytes(bytes_per_second.max(0.0) as u64))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, description: &str) -> CaptureDeviceInfo {
        CaptureDeviceInfo {
            name: name.to_string(),
            description: Some(description.to_string()),
        }
    }

    #[test]
    fn physical_device_is_preferred_over_virtual_adapters() {
        let devices = vec![
            device("loop", "Adapter for loopback traffic capture"),
            device("vpn", "Tailscale Tunnel"),
            device("wifi", "Intel Wi-Fi 6 AX201"),
        ];
        assert_eq!(choose_preferred_device(&devices).as_deref(), Some("wifi"));
    }

    #[test]
    fn application_and_domain_sentinels_are_localized() {
        assert_eq!(display_application(UNKNOWN_APPLICATION), "未知应用");
        assert_eq!(display_domain(UNKNOWN_DOMAIN), "未知域名");
    }

    #[test]
    fn month_to_date_is_the_primary_period_label() {
        assert_eq!(period_label(TrafficPeriod::MonthToDate), "本月");
        assert_eq!(period_title(TrafficPeriod::MonthToDate), "本月累计流量");
    }

    #[test]
    fn theme_labels_and_palettes_are_distinct() {
        assert_eq!(theme_label(ThemeMode::Light), "明亮模式");
        assert_eq!(theme_label(ThemeMode::Dark), "深色模式");
        let light = Palette::for_theme(ThemeMode::Light);
        let dark = Palette::for_theme(ThemeMode::Dark);
        assert_ne!(light.bg, dark.bg);
        assert_ne!(light.text, dark.text);
    }

    #[test]
    fn byte_and_integer_formatters_are_stable() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_integer(1_234_567), "1,234,567");
    }

    #[test]
    fn metric_columns_follow_window_width() {
        assert_eq!(responsive_metric_columns(1300.0), 5);
        assert_eq!(responsive_metric_columns(900.0), 3);
        assert_eq!(responsive_metric_columns(600.0), 2);
    }

    #[test]
    fn long_text_is_shortened() {
        assert_eq!(shorten_text("abcdefghijkl", 6), "abcde…");
        assert_eq!(shorten_text("abc", 6), "abc");
    }
}
