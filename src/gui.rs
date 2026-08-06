use crate::app_storage::{ApplicationStorage, TrafficPeriod};
use crate::capture::{list_devices, CaptureDeviceInfo};
use crate::model::{
    TopApplicationRow, TopDomainRow, TrafficTotals, HISTORICAL_APPLICATION, UNKNOWN_APPLICATION,
    UNKNOWN_DOMAIN,
};
use crate::runtime::{run_live_with_shutdown, RunSummary, RuntimeConfig};
use crate::settings::{database_parent, product_data_dir, AppSettings};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const APP_TITLE: &str = "域流量管家";
const UI_TICK: Duration = Duration::from_millis(250);
const BG: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);
const PANEL: egui::Color32 = egui::Color32::from_rgb(23, 32, 51);
const CARD: egui::Color32 = egui::Color32::from_rgb(30, 41, 59);
const CARD_HOVER: egui::Color32 = egui::Color32::from_rgb(38, 52, 75);
const BORDER: egui::Color32 = egui::Color32::from_rgb(55, 70, 94);
const TEXT: egui::Color32 = egui::Color32::from_rgb(235, 241, 250);
const MUTED: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);
const BLUE: egui::Color32 = egui::Color32::from_rgb(59, 130, 246);
const CYAN: egui::Color32 = egui::Color32::from_rgb(34, 211, 238);
const GREEN: egui::Color32 = egui::Color32::from_rgb(52, 211, 153);
const AMBER: egui::Color32 = egui::Color32::from_rgb(245, 158, 11);
const RED: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([1080.0, 680.0]),
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
    result_rx: Receiver<Result<RunSummary, String>>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    fn request_stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn try_result(&self) -> Option<Result<RunSummary, String>> {
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
            handle.join().map_err(|_| "抓包线程发生异常。".to_string())?;
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
    application_search: String,
    applications: Vec<TopApplicationRow>,
    selected_application: Option<String>,
    domains: Vec<TopDomainRow>,
    totals: TrafficTotals,
    state: CaptureState,
    capture: Option<CaptureWorker>,
    last_summary: Option<RunSummary>,
    last_refresh: Instant,
    last_rate_sample: Instant,
    last_total_bytes: u64,
    bytes_per_second: f64,
    notice: Option<String>,
}

impl DashboardApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        install_chinese_font(&creation_context.egui_ctx);
        configure_style(&creation_context.egui_ctx);

        let settings = AppSettings::load();
        let now = Instant::now();
        let mut app = Self {
            devices: Vec::new(),
            selected_device: settings.selected_device,
            database_path: settings.database_path.to_string_lossy().to_string(),
            period: settings.period,
            row_limit: settings.row_limit,
            auto_refresh: settings.auto_refresh,
            refresh_seconds: settings.refresh_seconds,
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
        };
        app.refresh_devices();
        app.refresh_data(false);
        app
    }

    fn settings(&self) -> AppSettings {
        AppSettings {
            selected_device: self.selected_device.clone(),
            database_path: PathBuf::from(self.database_path.trim()),
            period: self.period,
            row_limit: self.row_limit,
            auto_refresh: self.auto_refresh,
            refresh_seconds: self.refresh_seconds,
        }
    }

    fn save_settings(&self) {
        let _ = self.settings().save();
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
            let domains = storage.top_domains(self.period, selected, self.row_limit)?;
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
            if let Err(error) = std::process::Command::new("explorer.exe").arg(&folder).spawn() {
                self.notice = Some(format!("无法打开数据目录：{error}"));
            }
        }

        #[cfg(not(windows))]
        {
            self.notice = Some(format!("数据目录：{}", folder.display()));
        }
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "抓包控制", "选择网卡后即可持续记录，无需命令行");
        card(ui, |ui| {
            ui.label(egui::RichText::new("联网网卡").color(MUTED));
            let selected_text = self
                .selected_device
                .as_ref()
                .and_then(|name| self.devices.iter().find(|device| &device.name == name))
                .map(device_short_label)
                .unwrap_or_else(|| "请选择网卡".to_string());

            let enabled = self.capture.is_none();
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("capture-adapter")
                    .selected_text(selected_text)
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
                egui::Button::new(egui::RichText::new("▶ 开始记录流量").strong().color(TEXT))
                    .fill(BLUE)
            } else {
                egui::Button::new(egui::RichText::new("■ 停止并安全保存").strong().color(TEXT))
                    .fill(AMBER)
            };
            let clicked = ui
                .add_sized([ui.available_width(), 42.0], capture_button)
                .clicked();
            if clicked {
                if self.capture.is_none() {
                    self.start_capture();
                } else {
                    self.stop_capture();
                }
            }
            ui.add_space(6.0);
            status_badge(ui, &self.state);
            ui.label(
                egui::RichText::new("Npcap 抓包通常需要以管理员身份运行。")
                    .small()
                    .color(MUTED),
            );
        });

        ui.add_space(14.0);
        section_title(ui, "数据保存", "关闭和重启不会清空，继续写入同一数据库");
        card(ui, |ui| {
            ui.label(egui::RichText::new("SQLite 数据库").color(MUTED));
            ui.add_enabled(
                self.capture.is_none(),
                egui::TextEdit::singleline(&mut self.database_path)
                    .desired_width(ui.available_width()),
            );
            ui.horizontal(|ui| {
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
                egui::RichText::new("✓ 数据按天累加；本月累计会跨重启保留。")
                    .small()
                    .color(GREEN),
            );
        });

        ui.add_space(14.0);
        section_title(ui, "显示设置", "筛选统计周期与刷新频率");
        card(ui, |ui| {
            ui.label(egui::RichText::new("统计周期").color(MUTED));
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
            section_title(ui, "最近一次抓包", "安全停止后的运行摘要");
            card(ui, |ui| {
                summary_row(ui, "捕获数据包", summary.captured_packets);
                summary_row(ui, "纳入统计", summary.accepted_packets);
                summary_row(ui, "应用已归因", summary.attributed_packets);
                summary_row(ui, "应用未归因", summary.attribution_misses);
                summary_row(ui, "解析错误", summary.parse_errors);
                summary_row(ui, "写入批次", summary.submitted_batches);
            });
        }

        if let Some(notice) = &self.notice {
            ui.add_space(12.0);
            let color = if matches!(self.state, CaptureState::Failed(_)) {
                RED
            } else {
                MUTED
            };
            card(ui, |ui| {
                ui.label(egui::RichText::new(notice).color(color));
            });
        }
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui) {
        let app_coverage = percentage(
            self.totals.bytes.saturating_sub(self.totals.unknown_application_bytes),
            self.totals.bytes,
        );
        let domain_coverage = percentage(
            self.totals.bytes.saturating_sub(self.totals.unknown_domain_bytes),
            self.totals.bytes,
        );

        ui.horizontal_wrapped(|ui| {
            metric_card(ui, "本期流量", &format_bytes(self.totals.bytes), BLUE);
            metric_card(ui, "数据包", &format_integer(self.totals.packets), CYAN);
            metric_card(ui, "应用归因率", &format!("{app_coverage:.1}%"), GREEN);
            metric_card(ui, "域名识别率", &format!("{domain_coverage:.1}%"), AMBER);
            metric_card(ui, "实时写入", &format_rate(self.bytes_per_second), CYAN);
        });

        ui.add_space(14.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new(period_title(self.period)).color(TEXT));
                ui.label(
                    egui::RichText::new("数据来自持久化 SQLite，关闭程序后仍会保留")
                        .small()
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_sized(
                        [240.0, 30.0],
                        egui::TextEdit::singleline(&mut self.application_search)
                            .hint_text("搜索应用，例如 chrome、微信"),
                    );
                });
            });
        });

        ui.add_space(12.0);
        let available = ui.available_size();
        ui.columns(2, |columns| {
            columns[0].set_min_width((available.x * 0.42).max(360.0));
            self.render_applications(&mut columns[0]);
            self.render_domains(&mut columns[1]);
        });
    }

    fn render_applications(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("应用流量排行").color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} 个应用", self.applications.len()))
                            .color(MUTED),
                    );
                });
            });
            ui.label(
                egui::RichText::new("先选择应用，再查看该应用访问的域名。")
                    .small()
                    .color(MUTED),
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
                BLUE,
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
                        let selected = self.selected_application.as_deref()
                            == Some(row.application.as_str());
                        let color = if row.application == UNKNOWN_APPLICATION {
                            AMBER
                        } else if row.application == HISTORICAL_APPLICATION {
                            MUTED
                        } else {
                            BLUE
                        };
                        if application_row(
                            ui,
                            &display_application(&row.application),
                            row.bytes,
                            row.packets,
                            row.bytes as f32 / max_bytes as f32,
                            selected,
                            color,
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

    fn render_domains(&self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            let app_title = self
                .selected_application
                .as_deref()
                .map(display_application)
                .unwrap_or_else(|| "全部应用".to_string());
            ui.heading(egui::RichText::new(format!("{app_title} · 访问域名")).color(TEXT));
            ui.label(
                egui::RichText::new("HTTPS 域名来自 TLS ClientHello；QUIC、ECH 或漏抓握手会显示为未知域名。")
                    .small()
                    .color(MUTED),
            );
            ui.add_space(8.0);

            if self.domains.is_empty() {
                empty_state(ui);
                return;
            }

            let max_bytes = self.domains.first().map_or(1, |row| row.bytes.max(1));
            egui::ScrollArea::vertical()
                .id_salt("domains-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("domain-table")
                        .striped(true)
                        .min_col_width(80.0)
                        .show(ui, |ui| {
                            ui.strong("域名");
                            ui.strong("流量");
                            ui.strong("数据包");
                            ui.end_row();

                            for row in &self.domains {
                                let label = display_domain(&row.domain);
                                ui.label(shorten_text(&label, 42)).on_hover_text(&label);
                                ui.horizontal(|ui| {
                                    let ratio = row.bytes as f32 / max_bytes as f32;
                                    ui.add(
                                        egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                                            .desired_width(130.0)
                                            .fill(if row.domain == UNKNOWN_DOMAIN {
                                                AMBER
                                            } else {
                                                BLUE
                                            })
                                            .text(format_bytes(row.bytes)),
                                    );
                                });
                                ui.label(format_integer(row.packets));
                                ui.end_row();
                            }
                        });
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

        egui::TopBottomPanel::top("header")
            .exact_height(72.0)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(22, 12))
                    .stroke(egui::Stroke::new(1.0, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.heading(
                            egui::RichText::new("域流量管家")
                                .size(24.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            egui::RichText::new("Windows 应用与域名流量仪表盘")
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        status_badge(ui, &self.state);
                    });
                });
            });

        egui::SidePanel::left("controls")
            .resizable(false)
            .exact_width(340.0)
            .frame(
                egui::Frame::default()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.render_sidebar(ui));
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| self.render_dashboard(ui));

        ctx.request_repaint_after(UI_TICK);
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(10, 16, 30);
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.hovered.bg_fill = CARD_HOVER;
    style.visuals.widgets.active.bg_fill = BLUE;
    style.visuals.widgets.inactive.fg_stroke.color = TEXT;
    style.visuals.widgets.hovered.fg_stroke.color = TEXT;
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

fn card<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(14))
        .show(ui, content)
        .inner
}

fn section_title(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.heading(egui::RichText::new(title).size(18.0).strong().color(TEXT));
    ui.label(egui::RichText::new(subtitle).small().color(MUTED));
    ui.add_space(6.0);
}

fn metric_card(ui: &mut egui::Ui, title: &str, value: &str, accent: egui::Color32) {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.set_min_width(155.0);
            ui.label(egui::RichText::new(title).small().color(MUTED));
            ui.label(egui::RichText::new(value).size(22.0).strong().color(accent));
        });
}

fn period_selector(ui: &mut egui::Ui, period: &mut TrafficPeriod) {
    ui.horizontal_wrapped(|ui| {
        for option in TrafficPeriod::ALL {
            ui.selectable_value(period, option, period_label(option));
        }
    });
}

fn application_row(
    ui: &mut egui::Ui,
    name: &str,
    bytes: u64,
    packets: u64,
    ratio: f32,
    selected: bool,
    color: egui::Color32,
) -> bool {
    let fill = if selected { CARD_HOVER } else { CARD };
    let response = egui::Frame::default()
        .fill(fill)
        .stroke(egui::Stroke::new(if selected { 1.5 } else { 0.5 }, if selected { color } else { BORDER }))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(shorten_text(name, 34)).strong().color(TEXT));
                    ui.label(
                        egui::RichText::new(format!("{} 个数据包", format_integer(packets)))
                            .small()
                            .color(MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format_bytes(bytes)).strong().color(color));
                });
            });
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .desired_width(ui.available_width())
                    .fill(color)
                    .show_percentage(false),
            );
        })
        .response;
    response.interact(egui::Sense::click()).clicked()
}

fn empty_state(ui: &mut egui::Ui) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new("暂无流量数据").size(20.0).strong().color(TEXT));
        ui.label(
            egui::RichText::new("开始抓包并访问网页后，应用与域名会自动出现在这里。")
                .color(MUTED),
        );
    });
}

fn status_badge(ui: &mut egui::Ui, state: &CaptureState) {
    let (text, color) = match state {
        CaptureState::Idle => ("未开始", MUTED),
        CaptureState::Running => ("● 正在抓包", GREEN),
        CaptureState::Stopping => ("● 正在安全保存", AMBER),
        CaptureState::Finished => ("已停止并保存", CYAN),
        CaptureState::Failed(_) => ("发生错误", RED),
    };
    let response = egui::Frame::default()
        .fill(color.gamma_multiply(0.16))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(16)
        .inner_margin(egui::Margin::symmetric(12, 5))
        .show(ui, |ui| ui.label(egui::RichText::new(text).strong().color(color)))
        .response;
    if let CaptureState::Failed(error) = state {
        response.on_hover_text(error);
    }
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: u64) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.monospace(format_integer(value));
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
    fn byte_and_integer_formatters_are_stable() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_integer(1_234_567), "1,234,567");
    }

    #[test]
    fn long_text_is_shortened() {
        let shortened = shorten_text("very-long-subdomain.example.com", 16);
        assert_eq!(shortened.chars().count(), 16);
        assert!(shortened.ends_with('…'));
    }
}
