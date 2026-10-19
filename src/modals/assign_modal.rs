//! 自动分配确认弹窗：新导入的模型将替换服务当前选择时征询用户。

use eframe::egui::{self, Color32, RichText};

use crate::RagGuiApp;
use crate::theme::palette;
use crate::ui::short_model_name;

impl RagGuiApp {
    /// 自动分配确认弹窗：新导入的模型将替换当前选择时征询用户。
    pub fn render_assign_confirm_modal(&mut self, ctx: &egui::Context) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let Some(confirm) = self.assign_confirm.clone() else {
            return;
        };

        let mut decision: Option<bool> = None;
        let title = if lang.is_zh() {
            format!("切换 {} 模型？", confirm.svc_type.display_name(lang))
        } else {
            format!("Switch the {} model?", confirm.svc_type.display_name(lang))
        };
        egui::Window::new(title)
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
    .show(ctx, |ui| {
        let current = self
            .service_mgr
            .services
            .iter()
            .find(|svc| svc.config.service_type == confirm.svc_type)
            .map(|svc| svc.config.model_file.clone())
            .unwrap_or_default();
        let text = if lang.is_zh() {
            format!(
                "刚导入的 {} 可以分配给{}服务。\n\n当前选择: {}\n切换为: {}",
                confirm.file_name,
                confirm.svc_type.display_name(lang),
                short_model_name(&current),
                short_model_name(&confirm.file_name)
            )
        } else {
            format!(
                "The imported {} can be assigned to the {} service.\n\nCurrent: {}\nSwitch to: {}",
                confirm.file_name,
                confirm.svc_type.display_name(lang),
                short_model_name(&current),
                short_model_name(&confirm.file_name)
            )
        };
        ui.label(text);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let switch_btn = egui::Button::new(
                RichText::new(lang.t("切换", "Switch")).color(Color32::WHITE),
            )
            .fill(p.primary);
            if ui.add(switch_btn).clicked() {
                decision = Some(true);
            }
            if ui
                .button(lang.t("仅导入，不切换", "Keep current"))
                .clicked()
            {
                decision = Some(false);
            }
        });
    });

        let Some(switch) = decision else {
            return;
        };
        self.assign_confirm = None;
        if switch {
            let label = self.apply_auto_assign(&confirm.file_name, confirm.svc_type);
            self.save_config();
            self.set_status(
                if lang.is_zh() {
                    format!("已切换为 {}（{}）", confirm.file_name, label)
                } else {
                    format!("Switched to {} ({})", confirm.file_name, label)
                },
                false,
            );
        }
    }
}
