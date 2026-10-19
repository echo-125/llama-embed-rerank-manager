//! 跨页面共用的界面小部件与纯函数。
//!
//! 这里只放不依赖 `RagGuiApp` 状态的绘制辅助与校验逻辑，弹窗与页面都可以复用。

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::lang::Language;
use crate::service::ServiceStatus;
use crate::theme::{CONTROL_HEIGHT, Palette};
/// 绘制一个环形百分比指标（内存 / 显存 / GPU 共用）。
pub fn render_ring_metric(
    ui: &mut egui::Ui,
    label: &str,
    percent: f32,
    detail: String,
    color: Color32,
    p: Palette,
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

/// 接口详情弹窗里的一行「标签 + 值」；`copyable` 为真时带复制按钮，返回是否被复制。
pub fn api_row(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    lang: Language,
    p: Palette,
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

/// 服务状态对应的指示灯颜色。
pub fn status_color(status: &ServiceStatus, p: Palette) -> Color32 {
    match status {
        ServiceStatus::Running { .. } => p.success,
        ServiceStatus::Starting { .. } => p.warning,
        ServiceStatus::Stopped => p.text_muted,
        ServiceStatus::Failed(_) => p.danger,
    }
}

/// 服务状态的中英文文案。
pub fn status_text(lang: Language, status: &ServiceStatus) -> String {
    match status {
        ServiceStatus::Running { .. } => lang.t("运行中", "Running").to_string(),
        ServiceStatus::Starting { .. } => lang.t("启动中…", "Starting…").to_string(),
        ServiceStatus::Stopped => lang.t("已停止", "Stopped").to_string(),
        ServiceStatus::Failed(_) => lang.t("失败", "Failed").to_string(),
    }
}

/// 校验「另存为」输入的新模型文件名；返回 None 表示合法。
pub fn validate_model_file_name(name: &str, current_name: &str, lang: Language) -> Option<String> {
    if name.is_empty() {
        return Some(
            lang.t("文件名不能为空", "File name cannot be empty")
                .to_string(),
        );
    }
    if name.contains('\\') || name.contains('/') {
        return Some(
            lang.t("文件名不能包含 \\ 或 /", "File name cannot contain \\ or /")
                .to_string(),
        );
    }
    if name.eq_ignore_ascii_case(current_name) {
        return Some(
            lang.t(
                "新文件名与现有文件相同",
                "New name matches the existing file",
            )
            .to_string(),
        );
    }
    None
}

/// 同名冲突「另存为」的默认名：在扩展名前追加 ` (1)`；已有序号则递增。
pub fn default_save_as_name(dest_name: &str) -> String {
    let (stem, ext) = match dest_name.rfind('.') {
        Some(pos) => (&dest_name[..pos], &dest_name[pos..]),
        None => (dest_name, ""),
    };
    // 若名字已以 " (N)" 结尾，递增 N。
    if let Some(close) = stem.rfind(')')
        && let Some(open) = stem[..close].rfind(" (")
        && let Ok(n) = stem[open + 2..close].parse::<u32>()
    {
        return format!("{} ({}){}", &stem[..open], n + 1, ext);
    }
    format!("{stem} (1){ext}")
}

/// 把过长的模型名截断成「前 23 字…」，用于下拉框与模型行。
pub fn short_model_name(name: &str) -> String {
    const MAX: usize = 24;
    if name.chars().count() > MAX {
        let mut short: String = name.chars().take(MAX - 1).collect();
        short.push('…');
        short
    } else {
        name.to_string()
    }
}

/// 统一样式的小号按钮（与 `CONTROL_HEIGHT` 对齐）。
pub fn small_btn(text: &'static str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).size(11.0))
        .small()
        .min_size(egui::vec2(0.0, CONTROL_HEIGHT))
}

/// 固定宽度的表格单元格容器。
pub fn table_cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 20.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            add(ui);
        },
    );
}

/// 卡片标题（加粗、主文字色、统一字号）。
pub fn section_title(ui: &mut egui::Ui, p: Palette, text: &str) {
    ui.label(RichText::new(text).strong().size(13.0).color(p.text_strong));
}

/// 加载系统中文字体；找不到任何候选字体时保持 egui 默认字体。
pub fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 依次尝试常见的中文字体文件；都不可用时保持 egui 默认字体。
    const FONT_CANDIDATES: [&str; 3] = [
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑
        r"C:\Windows\Fonts\simhei.ttf", // 中易黑体
        r"C:\Windows\Fonts\simsun.ttc", // 中易宋体
    ];
    for font_path in FONT_CANDIDATES {
        if let Ok(font_data) = std::fs::read(font_path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                Arc::new(egui::FontData::from_owned(font_data)),
            );

            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, "cjk".to_owned());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.insert(0, "cjk".to_owned());
            }
            break;
        }
    }

    ctx.set_fonts(fonts);
}
