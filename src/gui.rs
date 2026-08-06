use crate::capture::{list_devices, CaptureDeviceInfo};
use crate::model::{TopDomainRow, UNKNOWN_DOMAIN};
use crate::runtime::{run_live_with_shutdown, RunSummary, RuntimeConfig};
use crate::storage::Storage;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const APP_TITLE: &str = "win-domain-flow";
const DEFAULT_DB_PATH: &str = "domainflow.db";
const UI_TICK: Duration = Duration::from_millis(250);

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
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
                Some(Err("capture worker disconnected unexpectedly".to_string()))
            }
        }
    }

    fn join(&mut self) -> Result<(), String> {
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| "capture worker panicked".to_string())?;
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
    db_path: String,
    days: u32,
    limit: u32,
    auto_refresh: bool,
    refresh_seconds: u64,
    rows: Vec<TopDomainRow>,
    state: CaptureState,
    capture: Option<CaptureWorker>,
    last_summary: Option<RunSummary>,
    last_refresh: Instant,
    last_rate_sample: Instant,
    last_visible_bytes: u64,
    bytes_per_second: f64,
    notice: Option<String>,
}

impl DashboardApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        creation_context.egui_ctx.set_visuals(egui::Visuals::dark());

        let now = Instant::now();
        let mut app = Self {
            devices: Vec::new(),
            selected_device: None,
            db_path: DEFAULT_DB_PATH.to_string(),
            days: 1,
            limit: 30,
            auto_refresh: true,
            refresh_seconds: 1,
            rows: Vec::new(),
            state: CaptureState::Idle,
            capture: None,
            last_summary: None,
            last_refresh: now,
            last_rate_sample: now,
            last_visible_bytes: 0,
            bytes_per_second: 0.0,
            notice: None,
        };
        app.refresh_devices();
        app.refresh_rows(false);
        app
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
            }
            Err(error) => {
                self.devices.clear();
                self.selected_device = None;
                self.notice = Some(format!("Unable to enumerate Npcap devices: {error}"));
            }
        }
    }

    fn start_capture(&mut self) {
        if self.capture.is_some() {
            return;
        }

        let Some(interface) = self.selected_device.clone() else {
            self.notice = Some("Select a capture adapter first.".to_string());
            return;
        };

        let db_text = self.db_path.trim();
        if db_text.is_empty() {
            self.notice = Some("Database path must not be empty.".to_string());
            return;
        }

        let db_path = PathBuf::from(db_text);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (result_tx, result_rx) = mpsc::channel();
        let config = RuntimeConfig::default();

        let spawn_result = thread::Builder::new()
            .name("domainflow-gui-capture".to_string())
            .spawn(move || {
                let result = run_live_with_shutdown(
                    &interface,
                    &db_path,
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
                self.notice = None;
                self.refresh_rows(false);
                self.last_visible_bytes = visible_totals(&self.rows).0;
                self.last_rate_sample = Instant::now();
                self.bytes_per_second = 0.0;
            }
            Err(error) => {
                self.state = CaptureState::Failed(error.to_string());
                self.notice = Some(format!("Unable to start capture worker: {error}"));
            }
        }
    }

    fn stop_capture(&mut self) {
        if let Some(worker) = self.capture.as_ref() {
            worker.request_stop();
            self.state = CaptureState::Stopping;
            self.notice = Some("Stopping capture and flushing pending traffic...".to_string());
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
                self.notice =
                    Some("Capture stopped. Final traffic was flushed to SQLite.".to_string());
            }
            Err(error) => {
                self.state = CaptureState::Failed(error.clone());
                self.notice = Some(error);
            }
        }
        self.refresh_rows(true);
    }

    fn refresh_rows(&mut self, update_rate: bool) {
        let path = Path::new(self.db_path.trim());
        if self.db_path.trim().is_empty() || !path.is_file() {
            self.rows.clear();
            self.last_refresh = Instant::now();
            if update_rate {
                self.bytes_per_second = 0.0;
            }
            return;
        }

        match Storage::open(path)
            .and_then(|storage| storage.top_domains_recent(self.days, self.limit))
        {
            Ok(rows) => {
                self.rows = rows;
                self.notice = match &self.state {
                    CaptureState::Failed(_) => self.notice.take(),
                    _ => None,
                };
                if update_rate {
                    self.update_rate();
                }
            }
            Err(error) => {
                self.notice = Some(format!("Unable to read traffic database: {error}"));
            }
        }
        self.last_refresh = Instant::now();
    }

    fn update_rate(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_rate_sample).as_secs_f64();
        let visible_bytes = visible_totals(&self.rows).0;
        if elapsed > 0.0 {
            let growth = visible_bytes.saturating_sub(self.last_visible_bytes);
            self.bytes_per_second = growth as f64 / elapsed;
        }
        self.last_visible_bytes = visible_bytes;
        self.last_rate_sample = now;
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("Capture controls");
        ui.add_space(6.0);

        let selected_text = self
            .selected_device
            .as_ref()
            .and_then(|name| self.devices.iter().find(|device| &device.name == name))
            .map(device_label)
            .unwrap_or_else(|| "No adapter selected".to_string());

        egui::ComboBox::from_label("Capture adapter")
            .selected_text(selected_text)
            .width(285.0)
            .show_ui(ui, |ui| {
                for device in &self.devices {
                    let label = device_label(device);
                    ui.selectable_value(
                        &mut self.selected_device,
                        Some(device.name.clone()),
                        label,
                    );
                }
            });

        if ui.button("Refresh adapters").clicked() {
            self.refresh_devices();
        }

        ui.separator();
        ui.label("SQLite database");
        ui.add_enabled(
            self.capture.is_none(),
            egui::TextEdit::singleline(&mut self.db_path).desired_width(285.0),
        );

        ui.horizontal(|ui| {
            if self.capture.is_none() {
                if ui.button("Start capture").clicked() {
                    self.start_capture();
                }
            } else if ui.button("Stop and flush").clicked() {
                self.stop_capture();
            }
            status_badge(ui, &self.state);
        });

        ui.small("Npcap live capture normally requires an Administrator session.");

        ui.separator();
        ui.heading("Dashboard view");
        ui.horizontal(|ui| {
            ui.label("Days");
            if ui
                .add(egui::DragValue::new(&mut self.days).range(1..=3650))
                .changed()
            {
                self.refresh_rows(false);
            }
            ui.label("Rows");
            if ui
                .add(egui::DragValue::new(&mut self.limit).range(1..=100))
                .changed()
            {
                self.refresh_rows(false);
            }
        });
        ui.checkbox(&mut self.auto_refresh, "Auto refresh");
        ui.horizontal(|ui| {
            ui.label("Every");
            ui.add(egui::DragValue::new(&mut self.refresh_seconds).range(1..=30));
            ui.label("seconds");
        });
        if ui.button("Refresh now").clicked() {
            self.refresh_rows(true);
        }

        if let Some(summary) = &self.last_summary {
            ui.separator();
            ui.heading("Last capture summary");
            summary_row(ui, "Captured", summary.captured_packets);
            summary_row(ui, "Accepted", summary.accepted_packets);
            summary_row(ui, "Parse errors", summary.parse_errors);
            summary_row(ui, "Resolved flows", summary.resolved_flows);
            summary_row(ui, "Flush batches", summary.submitted_batches);
        }

        if let Some(notice) = &self.notice {
            ui.separator();
            ui.label(notice);
        }
    }

    fn render_dashboard(&self, ui: &mut egui::Ui) {
        let (visible_bytes, visible_packets, unknown_bytes) = visible_totals(&self.rows);
        let known_percentage = if visible_bytes == 0 {
            0.0
        } else {
            100.0 * visible_bytes.saturating_sub(unknown_bytes) as f64 / visible_bytes as f64
        };

        ui.horizontal_wrapped(|ui| {
            metric(ui, "Visible traffic", &format_bytes(visible_bytes));
            metric(ui, "Packets", &visible_packets.to_string());
            metric(ui, "Known-domain share", &format!("{known_percentage:.1}%"));
            metric(ui, "Database growth", &format_rate(self.bytes_per_second));
        });

        ui.add_space(10.0);
        ui.heading("Top domains");
        ui.small(format!(
            "Showing the top {} rows from the last {} day(s).",
            self.limit, self.days
        ));
        ui.add_space(6.0);

        if self.rows.is_empty() {
            ui.group(|ui| {
                ui.label("No traffic rows are available yet.");
                ui.small("Select an adapter and start capture, or point the dashboard at an existing database.");
            });
            return;
        }

        let max_bytes = self.rows.first().map_or(1, |row| row.bytes.max(1));
        for row in self.rows.iter().take(10) {
            traffic_bar(ui, row, max_bytes);
        }

        ui.add_space(12.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("domain-table")
                .striped(true)
                .min_col_width(80.0)
                .show(ui, |ui| {
                    ui.strong("#");
                    ui.strong("Domain");
                    ui.strong("Bytes");
                    ui.strong("Packets");
                    ui.end_row();

                    for (index, row) in self.rows.iter().enumerate() {
                        ui.label((index + 1).to_string());
                        ui.label(&row.domain);
                        ui.label(format_bytes(row.bytes));
                        ui.label(row.packets.to_string());
                        ui.end_row();
                    }
                });
        });
    }
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_capture();

        if self.auto_refresh
            && self.last_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds.max(1))
        {
            self.refresh_rows(true);
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("win-domain-flow");
                ui.separator();
                ui.label("Domain-level Windows traffic dashboard");
            });
        });

        egui::SidePanel::left("controls")
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| self.render_controls(ui));

        egui::CentralPanel::default().show(ctx, |ui| self.render_dashboard(ui));
        ctx.request_repaint_after(UI_TICK);
    }
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

fn device_label(device: &CaptureDeviceInfo) -> String {
    match device.description.as_deref() {
        Some(description) if !description.trim().is_empty() => {
            format!("{} | {}", description.trim(), device.name)
        }
        _ => device.name.clone(),
    }
}

fn visible_totals(rows: &[TopDomainRow]) -> (u64, u64, u64) {
    rows.iter().fold((0_u64, 0_u64, 0_u64), |mut totals, row| {
        totals.0 = totals.0.saturating_add(row.bytes);
        totals.1 = totals.1.saturating_add(row.packets);
        if row.domain == UNKNOWN_DOMAIN {
            totals.2 = totals.2.saturating_add(row.bytes);
        }
        totals
    })
}

fn metric(ui: &mut egui::Ui, title: &str, value: &str) {
    ui.group(|ui| {
        ui.set_min_width(160.0);
        ui.small(title);
        ui.heading(value);
    });
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: u64) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.monospace(value.to_string());
        });
    });
}

fn status_badge(ui: &mut egui::Ui, state: &CaptureState) {
    let (text, color) = match state {
        CaptureState::Idle => ("Idle", egui::Color32::GRAY),
        CaptureState::Running => ("Running", egui::Color32::LIGHT_GREEN),
        CaptureState::Stopping => ("Flushing", egui::Color32::YELLOW),
        CaptureState::Finished => ("Stopped", egui::Color32::LIGHT_BLUE),
        CaptureState::Failed(_) => ("Error", egui::Color32::LIGHT_RED),
    };
    ui.colored_label(color, text);
    if let CaptureState::Failed(error) = state {
        ui.on_hover_text(error);
    }
}

fn traffic_bar(ui: &mut egui::Ui, row: &TopDomainRow, max_bytes: u64) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [220.0, 20.0],
            egui::Label::new(shorten_domain(&row.domain, 34)),
        );

        let available = (ui.available_width() - 100.0).max(80.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(available, 18.0), egui::Sense::hover());
        let ratio = row.bytes as f32 / max_bytes as f32;
        let background = ui.visuals().widgets.inactive.bg_fill;
        let fill = if row.domain == UNKNOWN_DOMAIN {
            egui::Color32::from_rgb(170, 110, 55)
        } else {
            egui::Color32::from_rgb(65, 145, 225)
        };
        ui.painter().rect_filled(rect, 4.0, background);
        let filled = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * ratio.clamp(0.0, 1.0), rect.height()),
        );
        ui.painter().rect_filled(filled, 4.0, fill);
        response.on_hover_text(format!(
            "{}\n{} bytes\n{} packets",
            row.domain, row.bytes, row.packets
        ));
        ui.monospace(format_bytes(row.bytes));
    });
}

fn shorten_domain(domain: &str, max_chars: usize) -> String {
    let count = domain.chars().count();
    if count <= max_chars {
        return domain.to_string();
    }
    let prefix: String = domain.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{prefix}…")
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
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
    fn visible_totals_track_unknown_bytes_separately() {
        let rows = vec![
            TopDomainRow {
                domain: "example.com".to_string(),
                bytes: 900,
                packets: 9,
            },
            TopDomainRow {
                domain: UNKNOWN_DOMAIN.to_string(),
                bytes: 100,
                packets: 2,
            },
        ];
        assert_eq!(visible_totals(&rows), (1_000, 11, 100));
    }

    #[test]
    fn byte_formatter_uses_binary_units() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
    }

    #[test]
    fn long_domains_are_shortened() {
        let shortened = shorten_domain("very-long-subdomain.example.com", 16);
        assert_eq!(shortened.chars().count(), 16);
        assert!(shortened.ends_with('…'));
    }
}
