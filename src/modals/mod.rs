//! 各类弹窗。
//!
//! 每个文件是一组 `impl RagGuiApp` 的渲染方法；弹窗状态仍保存在 `RagGuiApp` 上，
//! 渲染时根据状态决定是否显示与如何处理用户决策。

mod api_modal;
mod assign_modal;
mod conflict_modal;
