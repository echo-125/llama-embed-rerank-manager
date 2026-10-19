//! 服务接口详情弹窗：展示地址、模型名、维度与协议，支持一键复制。

use eframe::egui::{self, Color32, CornerRadius, RichText};

use crate::RagGuiApp;
use crate::service::{ServiceStatus, ServiceType, model_alias};
use crate::theme::palette;
use crate::ui::{api_row, status_color, status_text};

impl RagGuiApp {
    /// 接口详情弹窗：展示地址、模型名、维度与协议，可一键复制。
    pub fn render_api_modal(&mut self, ctx: &egui::Context) {
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
}
