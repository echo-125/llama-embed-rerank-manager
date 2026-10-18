#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod icon;
mod lang;
mod model_mgr;
mod monitor;
mod service;
mod theme;
mod tray;

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::AppConfig;
use eframe::egui::{self, Color32, CornerRadius, RichText};
use lang::Language;
use model_mgr::{ImportJob, ModelFile, ModelKind, ModelManager, format_file_size};
use monitor::Monitor;
use service::{LogLevel, ServiceManager, ServiceStatus, ServiceType, find_llama_exe, model_alias};
use theme::{CONTROL_HEIGHT, apply_theme, card_frame, palette};
use tray::{SystemTray, TrayAction, check_tray_events};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const HF_GGUF_URL: &str = "https://huggingface.co/models?library=gguf";

/// 顶部 Tab 页签：(中文标签, 中文提示, 英文标签, 英文提示)。
const TABS: [(&str, &str, &str, &str); 4] = [
    (
        "模型运行",
        "资源占用与模型启停",
        "Model runtime",
        "Resource usage and model start/stop",
    ),
    (
        "模型文件",
        "浏览、导入与删除目录内的 GGUF 模型",
        "Model files",
        "Browse, import and delete GGUF models",
    ),
    (
        "运行日志",
        "各本地模型的实时输出",
        "Logs",
        "Live output from local models",
    ),
    (
        "设置",
        "端口、运行时路径与模型下载",
        "Settings",
        "Ports, runtime path and model downloads",
    ),
];

struct RagGuiApp {
    base_dir: PathBuf,
    config: AppConfig,
    service_mgr: ServiceManager,
    model_mgr: ModelManager,
    monitor: Monitor,
    tray: Option<SystemTray>,

    // App state
    models: Vec<ModelFile>,
    /// 与 `service_mgr.services` 按下标对齐的端口草稿。
    edit_ports: Vec<u16>,
    search_query: String,
    active_tab: usize,
    status_message: Option<(String, Instant, bool)>, // (msg, time, is_error)
    delete_confirm_file: Option<String>,
    api_modal_svc: Option<ServiceType>,
    /// 正在后台复制中的模型导入任务。
    import_job: Option<ImportJob>,

    // Log filters & state
    show_info: bool,
    show_warn: bool,
    show_error: bool,
    auto_scroll: bool,

    // Preferences
    minimize_to_tray: bool,
    auto_stop_on_exit: bool,
    is_window_visible: bool,

    // Timers
    last_sync: Instant,
}

fn has_gguf_files(dir: &std::path::Path) -> bool {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension() {
                if ext.to_string_lossy().eq_ignore_ascii_case("gguf") {
                    return true;
                }
            }
        }
    }
    false
}

fn resolve_base_dir() -> PathBuf {
    // 1. 优先检查当前工作目录
    if let Ok(cwd) = std::env::current_dir() {
        if has_gguf_files(&cwd) {
            return cwd;
        }
    }
    // 2. 检查可执行程序所在同级目录
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            if has_gguf_files(parent) {
                return parent.to_path_buf();
            }
        }
    }
    // 3. 检查已知的默认目录 E:\LlamaModels
    let default_path = PathBuf::from(r"E:\LlamaModels");
    if default_path.exists() {
        return default_path;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

impl RagGuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);

        let base_dir = resolve_base_dir();
        let config = AppConfig::load(&base_dir);
        apply_theme(&cc.egui_ctx, config.dark_mode);
        cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            config.language.window_title().to_owned(),
        ));

        let model_mgr = ModelManager::new(&base_dir);
        let models = model_mgr.scan_models();

        let llama_exe = config
            .llama_exe_path
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(find_llama_exe);
        let mut service_mgr = ServiceManager::new(&base_dir, llama_exe, config.language);
        service_mgr.set_configured_ports(&config.ports());
        for svc in &mut service_mgr.services {
            let svc_type = svc.config.service_type;
            if let Some(model_name) = config.model(svc_type) {
                let compatible = models
                    .iter()
                    .find(|m| m.file_name.eq_ignore_ascii_case(model_name))
                    .map(|m| m.kind.fits_service(svc_type))
                    .unwrap_or(true);
                if compatible {
                    svc.config.model_file = model_name.to_string();
                }
            }
        }

        let monitor = Monitor::new();
        let tray = SystemTray::new(config.language).ok();
        let edit_ports = service_mgr
            .services
            .iter()
            .map(|svc| svc.config.port)
            .collect();

        Self {
            base_dir,
            config,
            service_mgr,
            model_mgr,
            monitor,
            tray,
            models,
            edit_ports,
            search_query: String::new(),
            active_tab: 0,
            status_message: None,
            delete_confirm_file: None,
            api_modal_svc: None,
            import_job: None,
            show_info: true,
            show_warn: true,
            show_error: true,
            auto_scroll: true,
            minimize_to_tray: true,
            auto_stop_on_exit: true,
            is_window_visible: true,
            last_sync: Instant::now() - Duration::from_secs(5),
        }
    }

    fn lang(&self) -> Language {
        self.config.language
    }

    fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status_message = Some((msg.into(), Instant::now(), is_error));
    }

    fn refresh_models(&mut self) {
        let ignored = self.config.ignored_models.clone();
        self.models = self
            .model_mgr
            .scan_models()
            .into_iter()
            .filter(|model| {
                !ignored
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&model.file_name))
            })
            .collect();
    }

    /// 导入完成后，按模型类型自动分配给对应的服务。
    fn auto_assign_imported_model(&mut self, file_name: &str) -> Option<&'static str> {
        let kind = self
            .models
            .iter()
            .find(|model| model.file_name.eq_ignore_ascii_case(file_name))?
            .kind;
        let svc_type = kind.service_type()?;
        if let Some(svc) = self
            .service_mgr
            .services
            .iter_mut()
            .find(|svc| svc.config.service_type == svc_type)
        {
            svc.config.model_file = file_name.to_string();
        }
        self.save_config();
        Some(svc_type.display_name(self.config.language))
    }

    fn perform_exit(&mut self) -> ! {
        if self.auto_stop_on_exit {
            // 只停止本程序启动的实例；脚本启动的服务继续常驻。
            self.service_mgr.stop_owned_services();
        }
        std::process::exit(0);
    }

    /// 把服务面板中当前选择的模型同步回配置，再落盘。
    fn sync_models_to_config(&mut self) {
        let services: Vec<(ServiceType, String)> = self
            .service_mgr
            .services
            .iter()
            .map(|svc| (svc.config.service_type, svc.config.model_file.clone()))
            .collect();

        for (svc_type, model) in services {
            let model = (!model.trim().is_empty()).then_some(model);
            self.config.set_model(svc_type, model);
        }
    }

    fn save_config(&mut self) -> bool {
        self.sync_models_to_config();
        match self.config.save(&self.base_dir) {
            Ok(()) => true,
            Err(e) => {
                self.set_status(
                    format!(
                        "{}: {e}",
                        self.lang().t("保存配置失败", "Failed to save config")
                    ),
                    true,
                );
                false
            }
        }
    }

    fn apply_port_drafts(&mut self, persist: bool) -> bool {
        let mut seen = std::collections::HashSet::new();
        for &port in &self.edit_ports {
            if !seen.insert(port) {
                self.set_status(
                    self.lang().t(
                        "保存失败：不同模型不能使用相同端口",
                        "Cannot use the same port for multiple models",
                    ),
                    true,
                );
                return false;
            }
        }

        let ports: Vec<(ServiceType, u16)> = self
            .service_mgr
            .services
            .iter()
            .zip(&self.edit_ports)
            .map(|(svc, &port)| (svc.config.service_type, port))
            .collect();
        for (svc_type, port) in &ports {
            self.config.set_port(*svc_type, *port);
        }
        self.service_mgr.set_configured_ports(&ports);
        if persist {
            return self.save_config();
        }
        true
    }
}

impl eframe::App for RagGuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let lang = self.lang();
        let p = palette(self.config.dark_mode);

        // 1. Handle system tray events
        if let Some(action) = check_tray_events() {
            match action {
                TrayAction::ToggleShow => {
                    self.is_window_visible = !self.is_window_visible;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.is_window_visible));
                    if self.is_window_visible {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                TrayAction::Menu(id) => {
                    if let Some(tray) = &self.tray {
                        if id == tray.item_show.id() {
                            self.is_window_visible = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        } else if id == tray.item_start_all.id() {
                            self.service_mgr.start_all();
                            self.set_status(
                                lang.t("已发送全部模型启动指令", "Start signal sent to all models"),
                                false,
                            );
                        } else if id == tray.item_stop_all.id() {
                            self.service_mgr.stop_all();
                            self.set_status(
                                lang.t("已停止所有运行中的模型", "All running models stopped"),
                                false,
                            );
                        } else if id == tray.item_exit.id() {
                            self.perform_exit();
                        }
                    }
                }
            }
        }

        // 2. Handle close request
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.minimize_to_tray && self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.is_window_visible = false;
                self.set_status(
                    lang.t("已最小化到系统托盘", "Minimized to system tray"),
                    false,
                );
            } else {
                self.perform_exit();
            }
        }

        // 3. Periodic background updates (2 seconds refresh)
        let (emb_pid, rerank_pid) = (
            self.service_mgr.get_service_pid(ServiceType::Embedding),
            self.service_mgr.get_service_pid(ServiceType::Rerank),
        );

        self.monitor
            .update_if_needed(emb_pid, rerank_pid, Duration::from_millis(2000));

        if self.last_sync.elapsed() >= Duration::from_millis(2000) {
            self.last_sync = Instant::now();
            self.service_mgr.sync_with_existing_processes();
            self.refresh_models();
        }

        // 后台模型导入完成时回写状态（复制在独立线程中，不阻塞 UI）。
        let import_finished = self
            .import_job
            .as_ref()
            .map(|job| job.is_finished())
            .unwrap_or(false);
        if import_finished {
            let result = self.import_job.as_ref().and_then(|job| job.take_result());
            self.import_job = None;
            match result {
                Some(Ok(name)) => {
                    self.refresh_models();
                    let matched = self.auto_assign_imported_model(&name);
                    let msg = if let Some(label) = matched {
                        if lang.is_zh() {
                            format!("成功导入 {name}，已匹配到「{label}」")
                        } else {
                            format!("Imported {name}; matched to {label}")
                        }
                    } else {
                        if lang.is_zh() {
                            format!("成功导入 {name}（未识别为向量或重排模型）")
                        } else {
                            format!("Imported {name} (not recognized as embedding or reranker)")
                        }
                    };
                    self.set_status(msg, false);
                }
                Some(Err(e)) => {
                    self.set_status(
                        format!("{}: {e}", lang.t("导入失败", "Import failed")),
                        true,
                    );
                }
                None => {}
            }
        }

        // Repaint regularly for responsive logs and resource bars
        ctx.request_repaint_after(Duration::from_millis(500));

        // 4. Render Main Dashboard Layout
        ui.painter().rect_filled(ui.max_rect(), 0.0, p.panel_bg);

        // --- 顶部：Tab 导航 + 主题/语言切换（同一行，固定不滚动）---
        egui::Panel::top("header_panel")
            .frame(
                egui::Frame::new()
                    .fill(p.panel_bg)
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 4,
                        bottom: 0,
                    }),
            )
            .show(ui, |ui| {
                let full_width = ui.available_width();
                let mut toggle_theme = false;
                let mut toggle_language = false;

                ui.allocate_ui_with_layout(
                    egui::vec2(full_width, CONTROL_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 18.0;

                        for (index, tab) in TABS.iter().enumerate() {
                            let selected = self.active_tab == index;
                            let (label, hint) = if lang.is_zh() {
                                (tab.0, tab.1)
                            } else {
                                (tab.2, tab.3)
                            };

                            let mut text = RichText::new(label).size(13.0).color(if selected {
                                p.accent
                            } else {
                                p.text_secondary
                            });
                            if selected {
                                text = text.strong();
                            }

                            let response = ui
                                .add(egui::Label::new(text).sense(egui::Sense::click()))
                                .on_hover_text(hint);
                            if response.clicked() {
                                self.active_tab = index;
                            }

                            if selected {
                                let underline = egui::Rect::from_min_max(
                                    egui::pos2(response.rect.left(), response.rect.bottom() - 2.0),
                                    egui::pos2(response.rect.right(), response.rect.bottom()),
                                );
                                ui.painter().rect_filled(underline, 0.0, p.accent);
                            }
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;

                            let language_hint = lang.t("切换语言", "Switch language");
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(lang.switch_label())
                                            .size(11.5)
                                            .color(p.text_secondary),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(32.0, 22.0)),
                                )
                                .on_hover_text(language_hint)
                                .clicked()
                            {
                                toggle_language = true;
                            }

                            let theme_hint = if self.config.dark_mode {
                                lang.t("切换到浅色主题", "Switch to light theme")
                            } else {
                                lang.t("切换到深色主题", "Switch to dark theme")
                            };
                            if theme_toggle_button(ui, self.config.dark_mode, p)
                                .on_hover_text(theme_hint)
                                .clicked()
                            {
                                toggle_theme = true;
                            }
                        });
                    },
                );

                let (line, _) =
                    ui.allocate_exact_size(egui::vec2(full_width, 1.0), egui::Sense::hover());
                ui.painter().rect_filled(line, 0.0, p.border);

                if toggle_theme {
                    self.config.dark_mode = !self.config.dark_mode;
                    apply_theme(&ctx, self.config.dark_mode);
                    self.save_config();
                    ctx.request_repaint();
                }

                if toggle_language {
                    self.config.language = self.config.language.toggled();
                    self.service_mgr.set_language(self.config.language);
                    if let Some(tray) = &self.tray {
                        tray.set_language(self.config.language);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                        self.config.language.window_title().to_owned(),
                    ));
                    self.save_config();
                    ctx.request_repaint();
                }
            });

        // --- 底部：状态提示条（常驻占位，避免消息出现时内容上下跳动）---
        egui::Panel::bottom("status_panel")
            .frame(
                egui::Frame::new()
                    .fill(p.panel_bg)
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 2,
                        bottom: 2,
                    }),
            )
            .show(ui, |ui| self.render_status_bar(ui));

        // --- 中央内容区：按当前 Tab 渲染 ---
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.panel_bg)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);

                match self.active_tab {
                    // Tab 1 服务面板：资源监控 + 服务管控
                    0 => {
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                            )
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                self.render_resource_monitor(ui);
                                self.render_services_control(ui);
                            });
                    }
                    // Tab 2 模型管理
                    1 => self.render_model_management(ui),
                    // Tab 3 运行日志
                    2 => self.render_logs_view(ui),
                    // Tab 4 设置
                    _ => self.render_settings(ui),
                }
            });

        // Modals
        self.render_delete_modal(&ctx);
        self.render_api_modal(&ctx);
    }
}

fn draw_status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// 自绘亮/暗主题切换按钮，避免依赖字体中的太阳/月亮字符。
fn theme_toggle_button(ui: &mut egui::Ui, dark: bool, p: theme::Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(26.0, 22.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if response.hovered() {
            p.hover_bg
        } else {
            p.panel_bg
        };
        if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(5), bg);
        }
        let color = if response.hovered() {
            p.accent
        } else {
            p.text_secondary
        };
        let center = rect.center();

        if dark {
            let radius = 6.0;
            ui.painter().circle_filled(center, radius, color);
            ui.painter()
                .circle_filled(center + egui::vec2(3.0, -2.0), radius - 0.5, bg);
        } else {
            ui.painter().circle_filled(center, 3.2, color);
            for i in 0..8 {
                let angle = std::f32::consts::TAU * i as f32 / 8.0;
                let dir = egui::vec2(angle.cos(), angle.sin());
                ui.painter().line_segment(
                    [center + dir * 5.2, center + dir * 7.2],
                    egui::Stroke::new(1.4, color),
                );
            }
        }
    }
    response
}

impl RagGuiApp {
    fn render_resource_monitor(&mut self, ui: &mut egui::Ui) {
        let stats = self.monitor.get_stats();
        let lang = self.lang();
        let p = palette(self.config.dark_mode);

        card_frame(self.config.dark_mode).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                section_title(ui, p, lang.t("资源占用", "Resource usage"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if stats.gpu_available && !stats.gpu_name.is_empty() {
                        ui.label(
                            RichText::new(&stats.gpu_name)
                                .size(10.5)
                                .color(p.text_muted),
                        );
                    }
                });
            });

            ui.add_space(4.0);

            let ram_percent = stats.ram_percent.clamp(0.0, 100.0);
            let ram_color = if ram_percent > 85.0 {
                p.danger
            } else {
                p.accent
            };
            let ram_detail = format!(
                "{:.1}/{:.1} GB",
                stats.ram_used_mb as f32 / 1024.0,
                stats.ram_total_mb as f32 / 1024.0
            );

            let (vram_percent, vram_color, vram_detail) = if stats.gpu_available {
                let pct = stats.vram_percent.clamp(0.0, 100.0);
                (
                    pct,
                    if pct > 85.0 { p.danger } else { p.success },
                    format!(
                        "{:.1}/{:.1} GB",
                        stats.vram_used_mb as f32 / 1024.0,
                        stats.vram_total_mb as f32 / 1024.0
                    ),
                )
            } else {
                (0.0, p.text_muted, lang.t("未检测到", "No GPU").to_string())
            };

            let (gpu_percent, gpu_color, gpu_detail) = if stats.gpu_available {
                let pct = stats.gpu_util_percent as f32;
                (
                    pct,
                    if pct > 85.0 { p.danger } else { p.purple },
                    format!("{pct:.0}%"),
                )
            } else {
                (0.0, p.text_muted, "—".to_string())
            };

            let item_width = 150.0;
            let gap = 24.0;
            let total_width = item_width * 3.0 + gap * 2.0;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - total_width) * 0.5).max(0.0));
                for (idx, (label, percent, detail, color)) in [
                    (lang.t("内存", "Memory"), ram_percent, ram_detail, ram_color),
                    (
                        lang.t("显存", "VRAM"),
                        vram_percent,
                        vram_detail,
                        vram_color,
                    ),
                    (lang.t("GPU", "GPU"), gpu_percent, gpu_detail, gpu_color),
                ]
                .into_iter()
                .enumerate()
                {
                    if idx > 0 {
                        ui.add_space(gap);
                    }
                    ui.allocate_ui_with_layout(
                        egui::vec2(item_width, 40.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_min_width(item_width);
                            render_ring_metric(ui, label, percent, detail, color, p);
                        },
                    );
                }
            });
        });
    }

    fn render_services_control(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let model_options: Vec<Vec<String>> = self
            .service_mgr
            .services
            .iter()
            .map(|svc| {
                self.models
                    .iter()
                    .filter(|m| m.kind.fits_service(svc.config.service_type))
                    .map(|m| m.file_name.clone())
                    .collect()
            })
            .collect();

        let mut do_start_all = false;
        let mut do_stop_all = false;
        let mut do_restart_all = false;
        let mut port_edit_changed = false;
        let mut port_edit_done = false;
        let mut model_selection_changed = false;

        let mut action_start = None;
        let mut action_stop = None;
        let mut action_restart = None;
        let mut action_test = None;

        card_frame(self.config.dark_mode).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                section_title(ui, p, lang.t("模型控制", "Model control"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let start_all_btn = egui::Button::new(
                        RichText::new(lang.t("全部启动", "Start all"))
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(p.primary);
                    if ui.add(start_all_btn).clicked() {
                        do_start_all = true;
                    }
                    if ui.button(lang.t("全部停止", "Stop all")).clicked() {
                        do_stop_all = true;
                    }
                    if ui.button(lang.t("全部重启", "Restart all")).clicked() {
                        do_restart_all = true;
                    }
                });
            });

            ui.add_space(4.0);

            let svc_len = model_options.len();
            for (i, options) in model_options.iter().enumerate() {
                let svc = &mut self.service_mgr.services[i];
                let is_running = matches!(svc.status, ServiceStatus::Running { .. });
                let is_starting = matches!(svc.status, ServiceStatus::Starting { .. });
                let is_stopped = matches!(
                    svc.status,
                    ServiceStatus::Stopped | ServiceStatus::Failed(_)
                );
                let svc_type = svc.config.service_type;
                let port = svc.config.port;
                let pending_port = svc.pending_port;
                let dot_color = status_color(&svc.status, p);
                let state_text = status_text(lang, &svc.status);

                ui.horizontal(|ui| {
                    draw_status_dot(ui, dot_color);

                    ui.add_sized(
                        [78.0, CONTROL_HEIGHT],
                        egui::Label::new(
                            RichText::new(svc_type.display_name(lang))
                                .strong()
                                .size(12.0)
                                .color(p.text_strong),
                        ),
                    );

                    let state_response = ui.add_sized(
                        [58.0, CONTROL_HEIGHT],
                        egui::Label::new(RichText::new(&state_text).size(11.0).color(dot_color)),
                    );

                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if is_stopped {
                            let response = ui.add_sized(
                                [56.0, CONTROL_HEIGHT],
                                egui::DragValue::new(&mut self.edit_ports[i])
                                    .range(1024..=65535)
                                    .speed(1.0),
                            );
                            if response.changed() {
                                port_edit_changed = true;
                            }
                            if response.drag_stopped() || response.lost_focus() {
                                port_edit_done = true;
                            }
                        } else {
                            ui.add_sized(
                                [56.0, CONTROL_HEIGHT],
                                egui::Label::new(
                                    RichText::new(port.to_string())
                                        .monospace()
                                        .size(12.0)
                                        .color(p.text_secondary),
                                ),
                            );
                        }
                    });

                    if let Some(pending) = pending_port {
                        ui.label(
                            RichText::new(format!("→ {pending}"))
                                .size(10.5)
                                .color(p.warning),
                        )
                        .on_hover_text(lang.t(
                            "端口已保存，重启模型后生效",
                            "Port saved; restart the model to apply",
                        ));
                    }

                    let selected_model = svc.config.model_file.clone();
                    let selected_model_label = if selected_model.trim().is_empty() {
                        lang.t("选择模型", "Select model").to_string()
                    } else {
                        short_model_name(&selected_model)
                    };
                    let combo = egui::ComboBox::from_id_salt(format!("combo_svc_{i}"))
                        .width(260.0)
                        .selected_text(RichText::new(selected_model_label).size(11.0).color(p.text))
                        .show_ui(ui, |ui| {
                            for model_name in options {
                                if ui
                                    .selectable_value(
                                        &mut svc.config.model_file,
                                        model_name.clone(),
                                        model_name,
                                    )
                                    .changed()
                                {
                                    model_selection_changed = true;
                                }
                            }
                        });
                    if selected_model.trim().is_empty() {
                        combo
                            .response
                            .on_hover_text(lang.t("请先选择模型文件", "Select a model file first"));
                    } else {
                        combo.response.on_hover_text(&selected_model);
                    }

                    match &svc.status {
                        ServiceStatus::Running { pid } | ServiceStatus::Starting { pid } => {
                            state_response.on_hover_text(format!("PID {pid}"));
                        }
                        ServiceStatus::Failed(err) => {
                            state_response.on_hover_text(err);
                        }
                        ServiceStatus::Stopped => {}
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if is_running {
                            let stop_btn = egui::Button::new(
                                RichText::new(lang.t("停止", "Stop"))
                                    .size(11.0)
                                    .color(p.danger),
                            )
                            .fill(p.soft_danger)
                            .small()
                            .min_size(egui::vec2(0.0, CONTROL_HEIGHT));
                            if ui.add(stop_btn).clicked() {
                                action_stop = Some(i);
                            }
                            if ui.add(small_btn(lang.t("重启", "Restart"))).clicked() {
                                action_restart = Some(i);
                            }
                            if ui.add(small_btn(lang.t("测试", "Test"))).clicked() {
                                action_test = Some(i);
                            }
                        } else if is_starting {
                            let stop_btn = egui::Button::new(
                                RichText::new(lang.t("停止", "Stop"))
                                    .size(11.0)
                                    .color(p.danger),
                            )
                            .fill(p.soft_danger)
                            .small()
                            .min_size(egui::vec2(0.0, CONTROL_HEIGHT));
                            if ui.add(stop_btn).clicked() {
                                action_stop = Some(i);
                            }
                        } else {
                            let start_btn = egui::Button::new(
                                RichText::new(lang.t("启动", "Start"))
                                    .size(11.0)
                                    .color(Color32::WHITE),
                            )
                            .fill(p.primary)
                            .small()
                            .min_size(egui::vec2(0.0, CONTROL_HEIGHT));
                            if ui.add(start_btn).clicked() {
                                action_start = Some(i);
                            }
                        }

                        let api_btn =
                            egui::Button::new(RichText::new("API").size(11.0).color(p.accent))
                                .frame(false);
                        if ui.add(api_btn).clicked() {
                            self.api_modal_svc = Some(svc_type);
                        }
                    });
                });

                if i + 1 < svc_len {
                    ui.add_space(3.0);
                    ui.separator();
                    ui.add_space(3.0);
                }
            }
        });

        if model_selection_changed {
            self.save_config();
        }
        if port_edit_changed {
            self.apply_port_drafts(false);
        }
        if port_edit_done {
            self.apply_port_drafts(true);
        }

        if do_start_all && self.apply_port_drafts(true) {
            self.service_mgr.start_all();
            self.set_status(
                lang.t("已发送全部模型启动指令", "Start signal sent to all models"),
                false,
            );
        }
        if do_stop_all {
            self.service_mgr.stop_all();
            self.set_status(lang.t("已停止所有模型", "All models stopped"), false);
        }
        if do_restart_all && self.apply_port_drafts(true) {
            self.service_mgr.restart_all();
            self.set_status(
                lang.t(
                    "已发送全部模型重启指令",
                    "Restart signal sent to all models",
                ),
                false,
            );
        }
        if let Some(idx) = action_start
            && self.apply_port_drafts(true)
        {
            if let Err(e) = self.service_mgr.start_service(idx) {
                self.set_status(format!("{}: {e}", lang.t("启动失败", "Start failed")), true);
            } else {
                self.set_status(lang.t("模型启动指令已发送", "Start signal sent"), false);
            }
        }
        if let Some(idx) = action_stop {
            self.service_mgr.stop_service(idx);
            self.set_status(lang.t("模型已停止", "Model stopped"), false);
        }
        if let Some(idx) = action_restart
            && self.apply_port_drafts(true)
        {
            if let Err(e) = self.service_mgr.restart_service(idx) {
                self.set_status(
                    format!("{}: {e}", lang.t("重启失败", "Restart failed")),
                    true,
                );
            } else {
                self.set_status(lang.t("模型已成功重启", "Model restarted"), false);
            }
        }
        if let Some(idx) = action_test {
            match self.service_mgr.test_service(idx) {
                Ok(msg) => self.set_status(
                    format!("{}: {msg}", lang.t("测试通过", "Test passed")),
                    false,
                ),
                Err(err) => self.set_status(
                    format!("{}: {err}", lang.t("测试未通过", "Test failed")),
                    true,
                ),
            }
        }
    }

    fn render_model_management(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let mut do_import = false;
        let mut do_open_dir = false;
        let mut do_restore_removed = false;
        let mut to_delete = None;

        card_frame(self.config.dark_mode).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                section_title(ui, p, lang.t("模型文件", "Model files"));
                ui.label(
                    RichText::new(self.base_dir.to_string_lossy())
                        .size(11.0)
                        .color(p.text_muted),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let import_btn = egui::Button::new(
                        RichText::new(lang.t("+ 导入", "+ Import"))
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(p.primary);
                    // 同一时间只允许一个导入任务，避免多个大文件同时抢占磁盘。
                    if ui
                        .add_enabled(self.import_job.is_none(), import_btn)
                        .clicked()
                    {
                        do_import = true;
                    }

                    if ui.button(lang.t("打开目录", "Open folder")).clicked() {
                        do_open_dir = true;
                    }

                    if !self.config.ignored_models.is_empty() {
                        let restore_label = if lang.is_zh() {
                            format!("恢复已移除 ({})", self.config.ignored_models.len())
                        } else {
                            format!("Restore removed ({})", self.config.ignored_models.len())
                        };
                        if ui.button(restore_label).clicked() {
                            do_restore_removed = true;
                        }
                    }

                    ui.add_sized(
                        [150.0, CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text(lang.t("搜索模型...", "Search models...")),
                    );
                });
            });

            ui.add_space(4.0);

            // 后台导入进度（复制在独立线程中进行，界面保持可交互）。
            if let Some(job) = &self.import_job {
                let progress_text = if lang.is_zh() {
                    format!(
                        "正在导入 {}（{}/{}）",
                        job.file_name,
                        format_file_size(job.copied_bytes()),
                        format_file_size(job.total_bytes())
                    )
                } else {
                    format!(
                        "Importing {} ({}/{})",
                        job.file_name,
                        format_file_size(job.copied_bytes()),
                        format_file_size(job.total_bytes())
                    )
                };
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(progress_text)
                            .size(11.0)
                            .color(p.text_secondary),
                    );
                    ui.add(
                        egui::ProgressBar::new(job.progress())
                            .desired_width(160.0)
                            .desired_height(6.0)
                            .fill(p.accent),
                    );
                });
                ui.add_space(4.0);
            }

            let active_model_files = self.get_active_model_files();
            let query = self.search_query.trim().to_lowercase();
            let filtered_models: Vec<&ModelFile> = self
                .models
                .iter()
                .filter(|m| query.is_empty() || m.file_name.to_lowercase().contains(&query))
                .collect();

            let render_table = |ui: &mut egui::Ui, to_delete: &mut Option<String>| {
                const COL_FILE: f32 = 320.0;
                const COL_TYPE: f32 = 90.0;
                const COL_SIZE: f32 = 80.0;
                const COL_DIM: f32 = 80.0;
                const COL_ACTION: f32 = 60.0;

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for (width, header) in [
                        (COL_FILE, lang.t("文件名称", "File")),
                        (COL_TYPE, lang.t("模型类型", "Type")),
                        (COL_SIZE, lang.t("大小", "Size")),
                        (COL_DIM, lang.t("维度", "Dim")),
                        (COL_ACTION, lang.t("操作", "Action")),
                    ] {
                        table_cell(ui, width, |ui| {
                            ui.label(
                                RichText::new(header)
                                    .size(11.0)
                                    .strong()
                                    .color(p.text_muted),
                            );
                        });
                    }
                });
                ui.separator();

                for model in &filtered_models {
                    let is_in_use = active_model_files
                        .iter()
                        .any(|f| f.eq_ignore_ascii_case(&model.file_name));
                    let (type_name, bg_color, fg_color) = match model.kind {
                        ModelKind::Embedding => (model.kind.label(lang), p.soft_accent, p.accent),
                        ModelKind::Rerank => (model.kind.label(lang), p.soft_success, p.success),
                        ModelKind::Other => (model.kind.label(lang), p.soft_purple, p.purple),
                    };
                    let dim_text = model
                        .embedding_dim
                        .map(|dim| dim.to_string())
                        .unwrap_or_else(|| "—".to_string());

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;

                        table_cell(ui, COL_FILE, |ui| {
                            ui.label(
                                RichText::new(&model.file_name)
                                    .size(12.0)
                                    .color(p.text_strong),
                            );
                            if is_in_use {
                                ui.label(
                                    RichText::new(lang.t("(运行中)", "(in use)"))
                                        .size(10.5)
                                        .strong()
                                        .color(p.success),
                                );
                            }
                        });

                        table_cell(ui, COL_TYPE, |ui| {
                            egui::Frame::new()
                                .fill(bg_color)
                                .corner_radius(CornerRadius::same(3))
                                .inner_margin(egui::Margin::symmetric(6, 1))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(type_name)
                                            .size(10.5)
                                            .strong()
                                            .color(fg_color),
                                    );
                                });
                        });

                        table_cell(ui, COL_SIZE, |ui| {
                            ui.label(
                                RichText::new(&model.size_formatted)
                                    .monospace()
                                    .size(11.5)
                                    .color(p.text_secondary),
                            );
                        });

                        table_cell(ui, COL_DIM, |ui| {
                            ui.label(
                                RichText::new(dim_text)
                                    .monospace()
                                    .size(11.5)
                                    .color(p.text_secondary),
                            );
                        });

                        table_cell(ui, COL_ACTION, |ui| {
                            if is_in_use {
                                ui.label(
                                    RichText::new(lang.t("占用", "In use"))
                                        .size(11.0)
                                        .color(p.text_muted),
                                );
                            } else {
                                let remove_btn = egui::Button::new(
                                    RichText::new(lang.t("移除", "Remove"))
                                        .size(11.5)
                                        .color(p.warning),
                                )
                                .frame(false);
                                if ui.add(remove_btn).clicked() {
                                    *to_delete = Some(model.file_name.clone());
                                }
                            }
                        });
                    });
                    ui.separator();
                }
            };

            if filtered_models.is_empty() {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    if self.models.is_empty() {
                        ui.label(
                            RichText::new(lang.t(
                                "当前目录暂无 .gguf 模型文件，请放置模型至此或点击右上角「+ 导入」",
                                "No .gguf models in this folder. Copy a model here or use Import.",
                            ))
                            .size(12.0)
                            .color(p.text_muted),
                        );
                    } else {
                        ui.label(
                            RichText::new(format!(
                                "{} \"{}\"",
                                lang.t("未找到匹配的模型:", "No models match:"),
                                self.search_query
                            ))
                            .size(12.0)
                            .color(p.text_muted),
                        );
                    }
                });
                ui.add_space(12.0);
            } else if filtered_models.len() <= 6 {
                render_table(ui, &mut to_delete);
            } else {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        render_table(ui, &mut to_delete);
                    });
            }
        });

        if do_open_dir {
            let _ = std::process::Command::new("explorer")
                .arg(&self.base_dir)
                .spawn();
        }
        if do_restore_removed {
            self.config.ignored_models.clear();
            if self.save_config() {
                self.refresh_models();
                self.set_status(
                    lang.t("已恢复所有移除的模型", "All removed models restored"),
                    false,
                );
            }
        }
        if do_import {
            match self.model_mgr.import_model_dialog(lang) {
                Ok(Some(job)) => {
                    self.set_status(
                        if lang.is_zh() {
                            format!("正在后台导入模型: {}", job.file_name)
                        } else {
                            format!("Importing model in the background: {}", job.file_name)
                        },
                        false,
                    );
                    self.import_job = Some(job);
                }
                Ok(None) => {}
                Err(e) => {
                    self.set_status(
                        format!("{}: {e}", lang.t("导入失败", "Import failed")),
                        true,
                    );
                }
            }
        }
        if let Some(file_name) = to_delete {
            self.delete_confirm_file = Some(file_name);
        }
    }

    fn render_logs_view(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let mut do_clear = false;
        let mut do_export = false;

        card_frame(self.config.dark_mode).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                section_title(ui, p, lang.t("日志", "Logs"));
                ui.add_space(4.0);

                let info_label = RichText::new("INFO").size(11.0).color(p.text_secondary);
                let warn_label = RichText::new("WARN").size(11.0).color(p.text_secondary);
                let error_label = RichText::new("ERROR").size(11.0).color(p.text_secondary);

                ui.checkbox(&mut self.show_info, info_label);
                ui.checkbox(&mut self.show_warn, warn_label);
                ui.checkbox(&mut self.show_error, error_label);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let export_btn = egui::Button::new(
                        RichText::new(lang.t("导出", "Export"))
                            .size(11.5)
                            .color(p.text_secondary),
                    )
                    .frame(false);
                    if ui.add(export_btn).clicked() {
                        do_export = true;
                    }

                    let clear_btn = egui::Button::new(
                        RichText::new(lang.t("清空", "Clear"))
                            .size(11.5)
                            .color(p.text_secondary),
                    )
                    .frame(false);
                    if ui.add(clear_btn).clicked() {
                        do_clear = true;
                    }

                    let scroll_label = RichText::new(lang.t("自动滚动", "Auto scroll"))
                        .size(11.0)
                        .color(p.text_secondary);
                    ui.checkbox(&mut self.auto_scroll, scroll_label);
                });
            });

            ui.add_space(3.0);
            ui.separator();
            ui.add_space(3.0);

            let mut log_items = Vec::new();
            if let Ok(l) = self.service_mgr.logs.lock() {
                for item in l.iter() {
                    let allow = match item.level {
                        LogLevel::Info => self.show_info,
                        LogLevel::Warn => self.show_warn,
                        LogLevel::Error => self.show_error,
                    };
                    if allow {
                        log_items.push(item.clone());
                    }
                }
            }

            let log_height = (ui.available_height() - 4.0).max(120.0);
            let mut scroll_area = egui::ScrollArea::vertical()
                .max_height(log_height)
                .auto_shrink([false, false]);
            if self.auto_scroll {
                scroll_area = scroll_area.stick_to_bottom(true);
            }

            scroll_area.show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 2.0);

                if log_items.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(lang.t(
                                "暂无符合筛选条件的运行日志...",
                                "No logs match the current filters...",
                            ))
                            .size(12.0)
                            .color(p.text_muted),
                        );
                    });
                    ui.add_space(24.0);
                } else {
                    for item in &log_items {
                        let text_color = match item.level {
                            LogLevel::Info => p.text,
                            LogLevel::Warn => p.warning,
                            LogLevel::Error => p.danger,
                        };
                        let prefix_color = if item.service == "Embedding" {
                            p.accent
                        } else {
                            p.success
                        };

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("[{}]", item.service))
                                    .monospace()
                                    .size(10.5)
                                    .color(prefix_color),
                            );
                            ui.label(
                                RichText::new(&item.text)
                                    .monospace()
                                    .size(11.0)
                                    .color(text_color),
                            );
                        });
                    }
                }
            });
        });

        if do_clear {
            if let Ok(mut l) = self.service_mgr.logs.lock() {
                l.clear();
            }
            self.set_status(lang.t("已清空运行日志", "Logs cleared"), false);
        }

        if do_export
            && let Some(dest) = rfd::FileDialog::new()
                .set_title(lang.t("导出运行日志", "Export logs"))
                .set_file_name("rag-servers.log")
                .save_file()
        {
            let mut text = String::new();
            if let Ok(l) = self.service_mgr.logs.lock() {
                for item in l.iter() {
                    text.push_str(&format!("[{}] {}\n", item.service, item.text));
                }
            }
            if let Err(e) = fs::write(&dest, text) {
                self.set_status(
                    format!("{}: {e}", lang.t("导出日志失败", "Failed to export logs")),
                    true,
                );
            } else {
                self.set_status(
                    format!(
                        "{}: {}",
                        lang.t("已成功导出日志至", "Logs exported to"),
                        dest.display()
                    ),
                    false,
                );
            }
        }
    }

    fn render_status_bar(&mut self, ui: &mut egui::Ui) {
        let p = palette(self.config.dark_mode);
        if let Some((msg, created_at, is_error)) = &self.status_message
            && created_at.elapsed() < Duration::from_secs(5)
        {
            ui.horizontal(|ui| {
                let color = if *is_error { p.danger } else { p.accent };
                ui.colored_label(color, "●");
                ui.label(RichText::new(msg).size(11.5).strong().color(if *is_error {
                    p.danger
                } else {
                    p.text_strong
                }));
            });
        }
    }

    fn render_delete_modal(&mut self, ctx: &egui::Context) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        if let Some(target_file) = self.delete_confirm_file.clone() {
            egui::Window::new(lang.t("确认移除导入", "Remove import"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    let text = if lang.is_zh() {
                        format!(
                            "将从模型列表中移除以下模型，但不会删除磁盘文件：\n\n{target_file}"
                        )
                    } else {
                        format!(
                            "This hides the model from the list but keeps the file on disk:\n\n{target_file}"
                        )
                    };
                    ui.label(text);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(lang.t("移除", "Remove"))
                                        .color(Color32::WHITE),
                                )
                                .fill(p.warning),
                            )
                            .clicked()
                        {
                            let in_use = self
                                .get_active_model_files()
                                .iter()
                                .any(|name| name.eq_ignore_ascii_case(&target_file));
                            if in_use {
                                self.set_status(
                                    lang.t(
                                        "模型正在运行，请先停止再移除",
                                        "Stop the model before removing it",
                                    ),
                                    true,
                                );
                            } else {
                                if !self
                                    .config
                                    .ignored_models
                                    .iter()
                                    .any(|name| name.eq_ignore_ascii_case(&target_file))
                                {
                                    self.config.ignored_models.push(target_file.clone());
                                }
                                for svc in &mut self.service_mgr.services {
                                    if svc.config.model_file.eq_ignore_ascii_case(&target_file)
                                        && !matches!(
                                            svc.status,
                                            ServiceStatus::Running { .. }
                                                | ServiceStatus::Starting { .. }
                                        )
                                    {
                                        svc.config.model_file.clear();
                                    }
                                }
                                self.save_config();
                                self.refresh_models();
                                self.set_status(
                                    if lang.is_zh() {
                                        format!(
                                            "已移除导入：{target_file}（文件仍保留在磁盘）"
                                        )
                                    } else {
                                        format!(
                                            "Removed import: {target_file} (file kept on disk)"
                                        )
                                    },
                                    false,
                                );
                            }
                            self.delete_confirm_file = None;
                        }
                        if ui.button(lang.t("取消", "Cancel")).clicked() {
                            self.delete_confirm_file = None;
                        }
                    });
                });
        }
    }

    fn render_api_modal(&mut self, ctx: &egui::Context) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let mut copied_label: Option<String> = None;
        let mut close = false;

        if let Some(svc_type) = self.api_modal_svc {
            let (port, model_file, status) = match self
                .service_mgr
                .services
                .iter()
                .find(|svc| svc.config.service_type == svc_type)
            {
                Some(svc) => (
                    svc.config.port,
                    svc.config.model_file.clone(),
                    svc.status.clone(),
                ),
                None => (
                    svc_type.default_port(),
                    svc_type.default_model().to_string(),
                    ServiceStatus::Stopped,
                ),
            };
            let status_col = status_color(&status, p);
            let status_txt = status_text(lang, &status);
            let base_url = format!("http://127.0.0.1:{port}");

            let (endpoint_path, protocol) = match svc_type {
                ServiceType::Embedding => (
                    "/v1/embeddings",
                    lang.t("OpenAI 兼容 · Embeddings", "OpenAI-compatible · Embeddings"),
                ),
                ServiceType::Rerank => (
                    "/v1/rerank",
                    lang.t(
                        "Jina / Cohere 兼容 · Rerank",
                        "Jina / Cohere-compatible · Rerank",
                    ),
                ),
            };
            let endpoint_url = format!("{base_url}{endpoint_path}");
            let health_url = format!("{base_url}/health");
            let title = if lang.is_zh() {
                format!("{} 接口详情", svc_type.display_name(lang))
            } else {
                format!("{} API details", svc_type.display_name(lang))
            };

            egui::Window::new(title)
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .default_width(500.0)
                .min_width(500.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(
                    egui::Frame::new()
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 3],
                            blur: 8,
                            spread: 0,
                            color: Color32::from_black_alpha(45),
                        })
                        .inner_margin(egui::Margin {
                            left: 10,
                            right: 14,
                            top: 6,
                            bottom: 8,
                        }),
                )
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);

                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(svc_type.display_name(lang))
                                .strong()
                                .size(13.0)
                                .color(p.text_strong),
                        );
                        ui.colored_label(status_col, RichText::new(status_txt).size(11.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(lang.t("关闭", "Close"))
                                            .size(11.0)
                                            .color(p.text_secondary),
                                    )
                                    .fill(p.surface)
                                    .stroke(egui::Stroke::new(1.0, p.border))
                                    .corner_radius(CornerRadius::same(5))
                                    .min_size(egui::vec2(44.0, 20.0)),
                                )
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                    ui.separator();

                    api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("模型类型", "Model type"),
                        svc_type.display_name(lang),
                        false,
                    );
                    let model_name = if model_file.trim().is_empty() {
                        lang.t("未选择", "Not selected").to_string()
                    } else {
                        model_alias(&model_file)
                    };
                    if api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("模型", "Model"),
                        &model_name,
                        !model_file.trim().is_empty(),
                    ) {
                        copied_label = Some(lang.t("模型", "Model").to_string());
                    }
                    if svc_type == ServiceType::Embedding {
                        let dim_text = self
                            .models
                            .iter()
                            .find(|model| model.file_name.eq_ignore_ascii_case(&model_file))
                            .and_then(|model| model.embedding_dim)
                            .map(|dim| dim.to_string())
                            .unwrap_or_else(|| lang.t("未知", "Unknown").to_string());
                        api_row(
                            ui,
                            ctx,
                            lang,
                            p,
                            lang.t("维度", "Dimension"),
                            &dim_text,
                            false,
                        );
                    }
                    api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("协议", "Protocol"),
                        protocol,
                        false,
                    );
                    if api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("基础地址", "Base URL"),
                        &base_url,
                        true,
                    ) {
                        copied_label = Some(lang.t("基础地址", "Base URL").to_string());
                    }
                    if api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("调用接口", "Endpoint"),
                        &endpoint_url,
                        true,
                    ) {
                        copied_label = Some(lang.t("调用接口", "Endpoint").to_string());
                    }
                    if api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("健康检查", "Health check"),
                        &health_url,
                        true,
                    ) {
                        copied_label = Some(lang.t("健康检查", "Health check").to_string());
                    }
                    api_row(
                        ui,
                        ctx,
                        lang,
                        p,
                        lang.t("鉴权", "Auth"),
                        lang.t("无（仅本机）", "None (localhost only)"),
                        false,
                    );
                });
        }

        if close {
            self.api_modal_svc = None;
        }
        if let Some(label) = copied_label {
            self.set_status(format!("{}: {label}", lang.t("已复制", "Copied")), false);
        }
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let dark = self.config.dark_mode;
        let mut save_ports = false;
        let mut redetect_llama = false;
        let mut pick_llama = false;
        let mut open_hf = false;

        egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 1. 服务端口
                card_frame(dark).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    section_title(ui, p, lang.t("模型端口", "Model ports"));
                    ui.add_space(4.0);

                    egui::Grid::new("settings_ports_grid")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            for (idx, svc_type) in ServiceType::ALL.iter().enumerate() {
                                ui.label(format!(
                                    "{} {}",
                                    svc_type.display_name(lang),
                                    lang.t("端口", "port")
                                ));
                                ui.add(
                                    egui::DragValue::new(&mut self.edit_ports[idx])
                                        .range(1024..=65535)
                                        .speed(1.0),
                                );
                                ui.end_row();
                            }
                        });

                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(lang.t(
                            "修改端口后，需要重启对应模型才能生效。",
                            "Restart the corresponding model after changing its port.",
                        ))
                        .size(11.0)
                        .color(p.text_muted),
                    );
                    ui.add_space(4.0);

                    if ui
                        .button(lang.t("保存端口配置", "Save ports"))
                        .clicked()
                    {
                        save_ports = true;
                    }
                });

                ui.add_space(6.0);

                // 2. llama.exe 路径
                card_frame(dark).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        section_title(ui, p, lang.t("llama.exe 路径", "llama.exe path"));
                        ui.add_space(4.0);

                        let found = self
                            .service_mgr
                            .get_llama_path()
                            .map(|path| path.exists())
                            .unwrap_or(false);
                        if found {
                            ui.colored_label(p.success, lang.t("已找到", "Found"));
                        } else {
                            ui.colored_label(p.danger, lang.t("未找到", "Not found"));
                        }
                    });
                    ui.add_space(4.0);

                    match self.service_mgr.get_llama_path() {
                        Some(path) => {
                            ui.label(
                                RichText::new(path.to_string_lossy())
                                    .monospace()
                                    .size(11.0)
                                    .color(p.text_secondary),
                            );
                        }
                        None => {
                            ui.colored_label(
                                p.danger,
                                lang.t(
                                    "未检测到 llama.exe，请安装 Llama 桌面端或手动指定路径。",
                                    "llama.exe was not found. Install the Llama desktop app or select its path.",
                                ),
                            );
                        }
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button(lang.t("重新检测", "Detect again")).clicked() {
                            redetect_llama = true;
                        }
                        if ui
                            .button(lang.t("手动选择路径", "Choose path"))
                            .clicked()
                        {
                            pick_llama = true;
                        }
                    });
                });

                ui.add_space(6.0);

                // 3. 环境信息
                card_frame(dark).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    section_title(ui, p, lang.t("环境信息", "Environment"));
                    ui.add_space(4.0);

                    let stats = self.monitor.get_stats();
                    let llama_found = self
                        .service_mgr
                        .get_llama_path()
                        .map(|path| path.exists())
                        .unwrap_or(false);

                    egui::Grid::new("settings_env_grid")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new(lang.t("显卡", "GPU")).color(p.text_secondary));
                            if stats.gpu_available && !stats.gpu_name.is_empty() {
                                ui.label(RichText::new(&stats.gpu_name).color(p.text));
                            } else {
                                ui.label(
                                    RichText::new(lang.t(
                                        "未检测到独立显卡",
                                        "No discrete GPU detected",
                                    ))
                                    .color(p.text_muted),
                                );
                            }
                            ui.end_row();

                            ui.label(
                                RichText::new("llama.exe").color(p.text_secondary),
                            );
                            if llama_found {
                                ui.colored_label(p.success, lang.t("已找到", "Found"));
                            } else {
                                ui.colored_label(p.danger, lang.t("未找到", "Not found"));
                            }
                            ui.end_row();
                        });
                });

                ui.add_space(6.0);

                // 4. 模型下载与安装说明
                card_frame(dark).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    section_title(ui, p, lang.t("模型下载与安装", "Model downloads"));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(lang.t(
                            "GGUF 模型可从 Hugging Face 下载。将向量或重排模型放入当前模型目录后，即可在「模型文件」中看到并分配。",
                            "GGUF models can be downloaded from Hugging Face. Put embedding or reranker models in the current folder; they will appear under Model files.",
                        ))
                        .size(11.5)
                        .color(p.text_secondary),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(self.base_dir.to_string_lossy())
                            .monospace()
                            .size(10.5)
                            .color(p.text_muted),
                    );
                    ui.add_space(4.0);

                    if ui
                        .button(lang.t(
                            "打开 Hugging Face GGUF 模型页",
                            "Open Hugging Face GGUF models",
                        ))
                        .clicked()
                    {
                        open_hf = true;
                    }
                });

                ui.add_space(6.0);

                // 5. 行为偏好
                card_frame(dark).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    section_title(ui, p, lang.t("行为偏好", "Behavior"));
                    ui.add_space(4.0);
                    ui.checkbox(
                        &mut self.minimize_to_tray,
                        lang.t(
                            "点击关闭按钮时最小化到系统托盘",
                            "Minimize to the system tray when closing",
                        ),
                    );
                    ui.checkbox(
                        &mut self.auto_stop_on_exit,
                        lang.t(
                            "退出管理器时自动停止本程序启动的 llama 模型进程",
                            "Stop models started by this app when exiting",
                        ),
                    );
                });
            });

        if save_ports && self.apply_port_drafts(true) {
            if self.service_mgr.has_pending_port_changes() {
                self.set_status(
                    lang.t(
                        "端口配置已保存，运行中的模型重启后生效",
                        "Ports saved; running models will use them after a restart",
                    ),
                    false,
                );
            } else {
                self.set_status(lang.t("端口配置已保存", "Ports saved"), false);
            }
        }

        if redetect_llama {
            match find_llama_exe() {
                Some(path) => {
                    self.service_mgr.set_llama_exe(path.clone());
                    self.config.llama_exe_path = Some(path.to_string_lossy().to_string());
                    if self.save_config() {
                        self.set_status(
                            if lang.is_zh() {
                                format!("已检测到 llama.exe: {}", path.display())
                            } else {
                                format!("llama.exe found: {}", path.display())
                            },
                            false,
                        );
                    }
                }
                None => self.set_status(
                    lang.t(
                        "未检测到 llama.exe，请确认已安装 Llama 桌面端",
                        "llama.exe not found. Make sure the Llama desktop app is installed.",
                    ),
                    true,
                ),
            }
        }

        if pick_llama
            && let Some(path) = rfd::FileDialog::new()
                .set_title(lang.t("选择 llama.exe", "Select llama.exe"))
                .add_filter(lang.t("可执行文件 (*.exe)", "Executable (*.exe)"), &["exe"])
                .pick_file()
        {
            self.service_mgr.set_llama_exe(path.clone());
            self.config.llama_exe_path = Some(path.to_string_lossy().to_string());
            if self.save_config() {
                self.set_status(
                    if lang.is_zh() {
                        format!("已设置 llama.exe 路径: {}", path.display())
                    } else {
                        format!("llama.exe path set: {}", path.display())
                    },
                    false,
                );
            }
        }

        if open_hf {
            match std::process::Command::new("cmd")
                .args(["/C", "start", "", HF_GGUF_URL])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
            {
                Ok(_) => self.set_status(
                    lang.t(
                        "已在浏览器中打开 Hugging Face",
                        "Opened Hugging Face in your browser",
                    ),
                    false,
                ),
                Err(e) => self.set_status(
                    format!(
                        "{}: {e}",
                        lang.t("打开浏览器失败", "Failed to open browser")
                    ),
                    true,
                ),
            }
        }
    }

    fn get_active_model_files(&self) -> Vec<String> {
        let mut active = Vec::new();
        for svc in &self.service_mgr.services {
            if matches!(
                svc.status,
                ServiceStatus::Running { .. } | ServiceStatus::Starting { .. }
            ) {
                active.push(svc.config.model_file.clone());
            }
        }
        active
    }
}

fn render_ring_metric(
    ui: &mut egui::Ui,
    label: &str,
    percent: f32,
    detail: String,
    color: Color32,
    p: theme::Palette,
) {
    ui.horizontal(|ui| {
        let size = 38.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        let center = rect.center();
        let radius = size * 0.5 - 3.5;
        ui.painter()
            .circle_stroke(center, radius, egui::Stroke::new(3.0, p.border));

        let pct = percent.clamp(0.0, 100.0);
        if pct > 0.0 {
            let start = -std::f32::consts::FRAC_PI_2;
            let sweep = std::f32::consts::TAU * pct / 100.0;
            let segments = ((pct / 4.0).ceil() as usize).max(6);
            let points: Vec<egui::Pos2> = (0..=segments)
                .map(|i| {
                    let angle = start + sweep * (i as f32 / segments as f32);
                    center + egui::vec2(angle.cos(), angle.sin()) * radius
                })
                .collect();
            ui.painter()
                .add(egui::Shape::line(points, egui::Stroke::new(3.0, color)));
        }

        ui.painter().text(
            center,
            egui::Align2::CENTER_CENTER,
            format!("{pct:.0}"),
            egui::FontId::proportional(10.0),
            p.text_strong,
        );

        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(11.0).strong().color(p.text));
            ui.label(RichText::new(detail).size(9.5).color(p.text_muted));
        });
    });
}

fn api_row(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    lang: Language,
    p: theme::Palette,
    label: &str,
    value: &str,
    copyable: bool,
) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_sized(
            [80.0, 18.0],
            egui::Label::new(RichText::new(label).size(11.0).color(p.text_secondary)),
        );
        if copyable {
            ui.allocate_ui_with_layout(
                egui::vec2(330.0, 18.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(value).monospace().size(10.5).color(p.text))
                            .truncate()
                            .selectable(true),
                    )
                    .on_hover_text(value);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(10.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(lang.t("复制", "Copy"))
                                        .size(10.5)
                                        .color(p.accent),
                                )
                                .frame(false)
                                .small()
                                .min_size(egui::vec2(0.0, 16.0)),
                            )
                            .clicked()
                        {
                            ctx.copy_text(value.to_owned());
                            clicked = true;
                        }
                    });
                },
            );
        } else {
            ui.label(RichText::new(value).size(11.0).color(p.text));
        }
    });
    ui.separator();
    clicked
}

fn status_color(status: &ServiceStatus, p: theme::Palette) -> Color32 {
    match status {
        ServiceStatus::Running { .. } => p.success,
        ServiceStatus::Starting { .. } => p.warning,
        ServiceStatus::Stopped => p.text_muted,
        ServiceStatus::Failed(_) => p.danger,
    }
}

fn status_text(lang: Language, status: &ServiceStatus) -> String {
    match status {
        ServiceStatus::Running { .. } => lang.t("运行中", "Running").to_string(),
        ServiceStatus::Starting { .. } => lang.t("启动中…", "Starting…").to_string(),
        ServiceStatus::Stopped => lang.t("已停止", "Stopped").to_string(),
        ServiceStatus::Failed(_) => lang.t("失败", "Failed").to_string(),
    }
}

fn short_model_name(name: &str) -> String {
    const MAX: usize = 24;
    if name.chars().count() > MAX {
        let mut short: String = name.chars().take(MAX - 1).collect();
        short.push('…');
        short
    } else {
        name.to_string()
    }
}

fn small_btn(text: &'static str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).size(11.0))
        .small()
        .min_size(egui::vec2(0.0, CONTROL_HEIGHT))
}

fn table_cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 20.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            add(ui);
        },
    );
}

fn section_title(ui: &mut egui::Ui, p: theme::Palette, text: &str) {
    ui.label(RichText::new(text).strong().size(13.0).color(p.text_strong));
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let font_path = std::path::Path::new(r"C:\Windows\Fonts\msyh.ttc");
    if let Ok(font_data) = std::fs::read(font_path) {
        fonts.font_data.insert(
            "msyh".to_owned(),
            Arc::new(egui::FontData::from_owned(font_data)),
        );

        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "msyh".to_owned());
        }
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.insert(0, "msyh".to_owned());
        }
    }

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    let window_icon = egui::IconData {
        rgba: icon::LOGO_128_RGBA.to_vec(),
        width: icon::LOGO_128_SIZE,
        height: icon::LOGO_128_SIZE,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 330.0])
            .with_min_inner_size([760.0, 300.0])
            .with_icon(Arc::new(window_icon))
            .with_title("向量/重排模型管理器"),
        ..Default::default()
    };

    eframe::run_native(
        "向量/重排模型管理器",
        options,
        Box::new(|cc| Ok(Box::new(RagGuiApp::new(cc)))),
    )
}
