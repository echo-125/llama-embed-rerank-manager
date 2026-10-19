use eframe::egui::{self, Color32, CornerRadius, Margin, Stroke, Visuals};

/// 全局控件高度基准：按钮、单行输入框、下拉框、复选框都以它对齐。
pub const CONTROL_HEIGHT: f32 = 28.0;

/// 一套主题用到的颜色，集中定义便于明暗切换。
#[derive(Clone, Copy)]
pub struct Palette {
    pub panel_bg: Color32,
    pub surface: Color32,
    pub input_bg: Color32,
    pub row_alt: Color32,
    pub border: Color32,

    pub text_strong: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,

    pub accent: Color32,
    pub primary: Color32,
    pub success: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub purple: Color32,

    /// 软背景色：徽章 / 危险按钮底色。部分字段当前仅个别控件使用。
    #[allow(dead_code)]
    pub soft_accent: Color32,
    #[allow(dead_code)]
    pub soft_success: Color32,
    pub soft_danger: Color32,
    #[allow(dead_code)]
    pub soft_purple: Color32,

    pub hover_bg: Color32,
    pub active_bg: Color32,
    pub selection_bg: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            panel_bg: Color32::from_rgb(27, 30, 36),
            surface: Color32::from_rgb(36, 40, 48),
            input_bg: Color32::from_rgb(23, 26, 32),
            row_alt: Color32::from_rgb(42, 47, 56),
            border: Color32::from_rgb(56, 63, 74),

            text_strong: Color32::from_rgb(243, 244, 246),
            text: Color32::from_rgb(203, 213, 225),
            text_secondary: Color32::from_rgb(148, 163, 184),
            text_muted: Color32::from_rgb(107, 114, 128),

            accent: Color32::from_rgb(96, 165, 250),
            primary: Color32::from_rgb(37, 99, 235),
            success: Color32::from_rgb(52, 211, 153),
            danger: Color32::from_rgb(248, 113, 113),
            warning: Color32::from_rgb(251, 191, 36),
            purple: Color32::from_rgb(167, 139, 250),

            soft_accent: Color32::from_rgb(30, 58, 95),
            soft_success: Color32::from_rgb(7, 60, 46),
            soft_danger: Color32::from_rgb(69, 26, 26),
            soft_purple: Color32::from_rgb(46, 36, 72),

            hover_bg: Color32::from_rgb(44, 52, 64),
            active_bg: Color32::from_rgb(51, 65, 85),
            selection_bg: Color32::from_rgb(30, 58, 95),
        }
    } else {
        Palette {
            panel_bg: Color32::from_rgb(243, 244, 246),
            surface: Color32::WHITE,
            input_bg: Color32::WHITE,
            row_alt: Color32::from_rgb(249, 250, 251),
            border: Color32::from_rgb(229, 231, 235),

            text_strong: Color32::from_rgb(15, 23, 42),
            text: Color32::from_rgb(51, 65, 85),
            text_secondary: Color32::from_rgb(107, 114, 128),
            text_muted: Color32::from_rgb(148, 163, 184),

            accent: Color32::from_rgb(37, 99, 235),
            primary: Color32::from_rgb(37, 99, 235),
            success: Color32::from_rgb(16, 185, 129),
            danger: Color32::from_rgb(220, 38, 38),
            warning: Color32::from_rgb(217, 119, 6),
            purple: Color32::from_rgb(139, 92, 246),

            soft_accent: Color32::from_rgb(239, 246, 255),
            soft_success: Color32::from_rgb(236, 253, 245),
            soft_danger: Color32::from_rgb(254, 226, 226),
            soft_purple: Color32::from_rgb(245, 243, 255),

            hover_bg: Color32::from_rgb(239, 246, 255),
            active_bg: Color32::from_rgb(219, 234, 254),
            selection_bg: Color32::from_rgb(219, 234, 254),
        }
    }
}

pub fn apply_theme(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    visuals.panel_fill = p.panel_bg;
    visuals.window_fill = p.surface;
    visuals.extreme_bg_color = p.input_bg;
    visuals.faint_bg_color = p.row_alt;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.override_text_color = Some(p.text_strong);
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.danger;

    visuals.widgets.noninteractive.bg_fill = p.surface;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_strong);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);

    visuals.widgets.inactive.bg_fill = p.surface;
    visuals.widgets.inactive.weak_bg_fill = p.surface;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(6);

    visuals.widgets.hovered.bg_fill = p.hover_bg;
    visuals.widgets.hovered.weak_bg_fill = p.hover_bg;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, p.accent);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(6);

    visuals.widgets.active.bg_fill = p.active_bg;
    visuals.widgets.active.weak_bg_fill = p.active_bg;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, p.text_strong);
    visuals.widgets.active.corner_radius = CornerRadius::same(6);

    visuals.selection.bg_fill = p.selection_bg;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);

    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.interact_size = egui::vec2(40.0, CONTROL_HEIGHT);
    });
}

/// 通用卡片包裹容器
pub fn card_frame(dark: bool) -> egui::Frame {
    let p = palette(dark);
    egui::Frame::new()
        .fill(p.surface)
        .inner_margin(Margin::same(10))
        .corner_radius(CornerRadius::same(6))
        .stroke(Stroke::new(1.0, p.border))
}
