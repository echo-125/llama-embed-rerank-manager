//! 模型文件页「确认移除导入」弹窗。
//!
//! 只把文件名写入 `ignored_models` 从列表隐藏，不删除磁盘文件。

use eframe::egui::{self, Color32, RichText};

use crate::RagGuiApp;
use crate::service::ServiceStatus;
use crate::theme::palette;

impl RagGuiApp {
    /// 「确认移除导入」弹窗：只隐藏、不删除磁盘文件。
    pub fn render_delete_modal(&mut self, ctx: &egui::Context) {
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
}
