//! 导入 / 导出同名冲突弹窗，以及冲突决策后的复制启动。
//!
//! 冲突策略：覆盖（运行中的模型禁止）/ 另存为 / 取消；决策后由
//! `start_copy_from_conflict` 重启后台复制任务。

use eframe::egui::{self, Color32, RichText};

use crate::RagGuiApp;
use crate::model_mgr::{CopyDirection, ImportConflict, format_file_size};
use crate::theme::{CONTROL_HEIGHT, palette};
use crate::ui::validate_model_file_name;

/// 冲突弹窗里用户的选择。
enum ConflictAction {
    Overwrite,
    SaveAs(String),
    Cancel,
}

impl RagGuiApp {
    /// 导入 / 导出同名冲突弹窗：覆盖 / 另存为 / 取消。
    pub fn render_import_conflict_modal(&mut self, ctx: &egui::Context) {
        let lang = self.lang();
        let p = palette(self.config.dark_mode);
        let Some(conflict) = self.import_conflict.clone() else {
            return;
        };

        let mut action: Option<ConflictAction> = None;
        egui::Window::new(lang.t("文件名冲突", "File name conflict"))
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
    .show(ctx, |ui| {
        let text = if lang.is_zh() {
            format!(
                "目标位置已存在同名文件：{}\n\n已有文件: {}\n新文件: {}\n\n可选择覆盖、另存为新名称，或取消本次{}。",
                conflict.dest_name,
                format_file_size(conflict.dest_size),
                format_file_size(conflict.source_size),
                lang.t("导入", "import")
            )
        } else {
            format!(
                "A file with the same name already exists: {}\n\nExisting: {}\nNew file: {}\n\nOverwrite, save under a new name, or cancel the {}.",
                conflict.dest_name,
                format_file_size(conflict.dest_size),
                format_file_size(conflict.source_size),
                lang.t("import", "import")
            )
        };
        ui.label(text);
        ui.add_space(8.0);

        // 覆盖守卫：目标被运行中的服务占用时禁止覆盖。
        let dest_in_use = self
            .get_active_model_files()
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&conflict.dest_name));

        // 另存为输入行
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.t("另存为:", "Save as:"))
                    .size(11.5)
                    .color(p.text_secondary),
            );
            ui.add_sized(
                [260.0, CONTROL_HEIGHT],
                egui::TextEdit::singleline(&mut self.conflict_save_as)
                    .hint_text(lang.t("输入新文件名", "Enter a new file name")),
            );
        });
        let save_as = self.conflict_save_as.trim().to_string();
        let save_as_invalid_reason =
validate_model_file_name(&save_as, &conflict.dest_name, lang);
        if let Some(reason) = &save_as_invalid_reason {
            ui.label(RichText::new(reason.clone()).size(10.5).color(p.danger));
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // 覆盖
            let overwrite_btn = egui::Button::new(
                RichText::new(lang.t("覆盖", "Overwrite")).color(Color32::WHITE),
            )
            .fill(p.danger);
            if ui
                .add_enabled(!dest_in_use, overwrite_btn)
                .on_disabled_hover_text(lang.t(
                    "该模型正在运行，请先停止再覆盖",
                    "Stop the running model before overwriting it",
                ))
                .clicked()
            {
                action = Some(ConflictAction::Overwrite);
            }
            // 另存为
            let can_save_as = save_as_invalid_reason.is_none();
            if ui
                .add_enabled(can_save_as, egui::Button::new(lang.t("另存为", "Save as")))
                .clicked()
            {
                action = Some(ConflictAction::SaveAs(save_as.clone()));
            }
            if ui.button(lang.t("取消", "Cancel")).clicked() {
                action = Some(ConflictAction::Cancel);
            }
        });
        if dest_in_use {
            ui.label(
                RichText::new(lang.t(
                    "目标模型正在运行，无法覆盖；可先停止模型或另存为新名称。",
                    "The target model is running; stop it first or save under a new name.",
                ))
                .size(11.0)
                .color(p.warning),
            );
        }
    });

        let Some(action) = action else {
            return;
        };
        self.import_conflict = None;
        match action {
            ConflictAction::Cancel => {}
            ConflictAction::Overwrite => {
                // 覆盖前再次检查占用（弹窗打开期间状态可能变化）。
                if self
                    .get_active_model_files()
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&conflict.dest_name))
                {
                    self.set_status(
                        lang.t(
                            "模型正在运行，无法覆盖，请先停止",
                            "The model is running; stop it before overwriting",
                        ),
                        true,
                    );
                    return;
                }
                self.start_copy_from_conflict(&conflict, &conflict.dest_name);
            }
            ConflictAction::SaveAs(new_name) => {
                // 另存为的目标也不能与目标目录中已有文件重名。
                let exists = conflict
                    .dest_path
                    .parent()
                    .map(|dir| dir.join(&new_name).exists())
                    .unwrap_or(false);
                if exists {
                    self.set_status(
                        if lang.is_zh() {
                            format!("目标目录中已存在同名文件: {new_name}")
                        } else {
                            format!("A file with the same name already exists: {new_name}")
                        },
                        true,
                    );
                } else {
                    self.start_copy_from_conflict(&conflict, &new_name);
                }
            }
        }
    }

    /// 冲突弹窗决策后启动（或重启）复制线程。
    pub fn start_copy_from_conflict(&mut self, conflict: &ImportConflict, dest_name: &str) {
        let lang = self.lang();
        // 冲突发生在导入或导出，取决于源路径是否在 base_dir 下。
        let is_import = conflict.source_path.parent() != Some(self.base_dir.as_path());
        // 覆盖与另存为都基于冲突记录里的原目标目录：另存为只替换文件名。
        let dest_path = conflict
            .dest_path
            .parent()
            .map(|dir| dir.join(dest_name))
            .unwrap_or_else(|| std::path::PathBuf::from(dest_name));
        if let Some(job) = self.model_mgr.copy_file_with_name(
            &conflict.source_path,
            &dest_path,
            dest_name,
            if is_import {
                CopyDirection::Import
            } else {
                CopyDirection::Export
            },
            lang,
        ) {
            self.set_status(
                if lang.is_zh() {
                    format!(
                        "正在后台{}: {}",
                        job.direction.progress_label(lang),
                        dest_name
                    )
                } else {
                    format!(
                        "{} in the background: {}",
                        job.direction.progress_label(lang),
                        dest_name
                    )
                },
                false,
            );
            self.import_job = Some(job);
        } else {
            self.set_status(
                lang.t(
                    "复制预检失败，未开始复制",
                    "Copy pre-check failed; nothing was started",
                ),
                true,
            );
        }
    }
}
