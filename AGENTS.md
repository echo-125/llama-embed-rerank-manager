# AGENTS.md

Rust/egui 桌面控制面板（`llama-rag-gui`）：**轻量的本地向量 / 重排模型启动器**，启动/停止/监控本机
llama.cpp（`llama-server`）的两个检索服务 —— 文本向量化（默认 9932，`Qwen3-Embedding-4B-Q6_K.gguf`）
与重排（默认 9933，`bge-reranker-v2-m3-F16.gguf`）。模型与运行时都在本机，不联网。
用户文档见 `README.md` 与 `docs/`。

## Product positioning

- **只做本地向量 / 重排模型的启动与管理。** 轻量、低占用、快启动是本项目的核心差异点；对标的是
  llama.cpp 桌面端只覆盖对话模型、而其他本地模型工具又过重留下的空档。
- **不扩展对话 / VLM / 翻译等生成式模型。** 这些能力交给 llama.cpp 桌面端或其他工具；本项目既不接入
  它们的服务行，也不为 OCR、ASR/TTS、图像生成引入 Python / ONNX / PyTorch 运行时或第二套推理引擎。
- **不做模型市场 / 自动下载。** 只扫描、导入本机已有的 GGUF，模型选择限定为向量或重排类型。
- **不做聊天客户端**：GUI 只负责进程、端口、健康检查、日志与资源占用。
- **服务按需启动**：停止/启动是常态操作，避免空载常驻模型。
- 功能取舍有疑问时，优先选择"少一个依赖、少一个常驻进程"的方案。

## Setup commands

- 构建：`cargo build --release`（Rust edition 2024，无独立依赖安装步骤）
- 开发运行：`cargo run`（debug 保留控制台；release 使用 windows_subsystem，隐藏控制台）
- 产物：`target/release/llama-rag-gui.exe`；根目录 `llama-rag-gui.exe` 是手动复制的发布副本
- 格式 / 检查：`cargo fmt` / `cargo clippy`
- 测试：`cargo test`
- 无 GUI 启停：`start-rag-servers.cmd [detach|status|stop]`（本地辅助脚本，不随仓库发布）
- 健康检查：`Invoke-RestMethod http://127.0.0.1:9932/health`

构建前置：
- Windows 10/11 x64
- Rust MSVC 工具链
- Windows SDK 的 `rc.exe`：`build.rs` 通过 `embed-resource` 把 `assets/llama.ico` 嵌入 exe
- `llama.exe` 由 Llama 桌面端提供（`%LOCALAPPDATA%\Microsoft\WindowsApps\llama.exe`），不随仓库分发

## Project layout

- `src/main.rs` — eframe 应用：模型运行 / 模型文件 / 运行日志 / 设置，托盘事件，明暗与中英文切换，弹窗
- `src/service.rs` — `ServiceType` / `ServiceConfig` / `ServiceManager`：向量与重排服务的拉起、健康检查、
  PID 文件、端口草稿与待生效端口、日志跟随
- `src/monitor.rs` — sysinfo 采样内存 / 显存 / GPU 利用率
- `src/model_mgr.rs` — 扫描 / 导入 GGUF；按文件名判断 Embedding / Rerank / Other
- `src/config.rs` — `AppConfig`：两个端口、两个模型、`ignored_models`、`llama_exe_path`、
  `dark_mode`、`language`，读写 `config.json`
- `src/lang.rs` — `Language::{Zh, En}` 与翻译辅助
- `src/icon.rs` — 内嵌 llama.app 图标的 RGBA 资源
- `src/tray.rs` — 系统托盘与中英文菜单
- `src/theme.rs` — 明暗 `Palette`、`apply_theme`、`card_frame`、控件高度
- `docs/` — 用户文档：定位、安装、模型、API、配置、FAQ、开发说明
- `assets/` — 图标源文件与构建资源：`llama-logo.svg`、`llama.ico`、`logo_32.rgba`、`logo_128.rgba`、`screenshot.png`
- `build.rs` — 嵌入 Windows exe 图标
- `start-gui.cmd` — 双击启动 GUI
- `start-rag-servers.ps1` / `.cmd` — 本地无 GUI 启停脚本（已忽略，不随仓库发布）；读取 `config.json` 的端口、模型与 llama 路径
- `gguf-info.ps1` — 本地 GGUF 信息工具（已忽略，不随仓库发布）
- `logs/` — 本地运行日志（已忽略，不随仓库发布）
- `.ref-m3u8/` — 外部 UI 参考代码，不参与构建，不要提交

## Architecture notes

- **两条服务管理路径**：GUI（公开）与本地 PowerShell 启动脚本（已忽略，不随仓库发布）。默认端口、模型名与
  llama-server 参数分别位于 `ServiceType` 和 `start-rag-servers.ps1` 的 `$services`；GUI 保存的端口、
  模型选择与 llama 路径写入 `config.json`，脚本启动时会读取端口与 llama 路径。改模型、参数或默认端口时
  仍要同时检查两边。
- **服务类型边界**：`ServiceType::ALL` 只有 `Embedding` 与 `Rerank` 两项，是服务顺序与配置持久化的唯一来源。
  新增能力若不属于这两类，不要往这里加。
- **端口状态**：停止状态下模型行可直接编辑端口（与设置页共享草稿值）；运行中的模型保存端口后会先记入
  `pending_port`，重启/停止后生效。启动/重启前会先应用草稿；两个端口必须互不相同。
- **模型选择**：模型行的下拉只显示与当前服务同类型的模型（由 `ModelKind::fits_service` 判断），选择会立即
  写入 `config.json`；启动前仍会校验文件存在。导入完成后会自动把模型分配给对应类型。
- **移除导入不是删除**：模型文件页的「移除」只把文件名写入 `ignored_models` 并从列表隐藏，磁盘文件保留；
  若该模型被停止中的服务选中会同时清空选择。页面提供「恢复已移除」入口。
- **接入模型名**：启动时传 `--alias`，值为所选模型文件名去掉 `.gguf`；API 弹窗展示同一名称。改规则要同步
  `model_alias`、`start_service` 的参数和 `start-rag-servers.ps1`。
- 基目录在运行时解析：当前工作目录 → 可执行文件同级目录 → `E:\LlamaModels`（见 `resolve_base_dir`）。
- 仅支持 Windows：使用 `std::os::windows::process::CommandExt` 与 `CREATE_NO_WINDOW` 隐藏子进程窗口，
  其他平台构建失败是预期行为，不要为跨平台做改造。
- `llama.exe` 是子命令模式，启动时第一个参数必须传 `serve`。
- **图标**：窗口/任务栏使用 `logo_128.rgba`，托盘使用 `logo_32.rgba`，exe 使用 `assets/llama.ico`
  （由 `build.rs` 嵌入）。界面内不再重复放 Logo。改图标时需同步重新生成这些资源和 ICO。
- **主题与语言**：偏好保存在 `config.json`。所有 UI 颜色必须走 `theme::palette(dark)`，不要新增硬编码
  浅色/深色值；所有用户可见文案通过 `Language::t(zh, en)` 或语言分支提供中英文。

## Code style

- rustfmt 默认风格，4 空格缩进，edition 2024
- 标识符用英文；注释用中文；用户可见 UI 文案必须中英双语
- 提交前跑 `cargo fmt`，不要引入新的 clippy 告警（旧模块仍有存量告警）
- 跨线程共享状态用 `Arc<Mutex<..>>`；进程拉起、目录扫描等阻塞操作放到 `std::thread` 里做，
  结果回写互斥量，不要阻塞 UI 刷新
- 颜色使用 `theme::Palette`；不要直接写 `Color32::from_rgb` 的界面颜色
- 弹窗和卡片使用 `card_frame(dark)` / `section_title`，保持间距、圆角和字号一致

## Testing instructions

- `cargo test`：`src/config.rs`（配置读写、字段默认值）、`src/model_mgr.rs`（模型类型判断）与
  `src/icon.rs`（图标资源字节数）。
- 新增测试放在对应模块的 `#[cfg(test)] mod tests`。
- 手工验证见 README 的「快速验证」：两个服务分别 `/health`，再调用 `/v1/embeddings` 与 `/v1/rerank`。
- 涉及端口、模型切换、服务参数、主题/语言切换的改动，必须实际启停一次服务并逐项验证。
- 主题/语言切换后要检查 `config.json` 是否持久化、重启后是否恢复。

## PR & commit conventions

- 从 `master` 分支开发；仓库目前仍无任何提交，首次提交请确认是否直接落在 `master`
- 提交信息用 conventional commits（`feat:` / `fix:` / `docs:` / `refactor:`）
- `.gitignore` 已忽略 `/target`、`/llama-rag-gui.exe`、`/config.json`、`*.gguf`、`*.part`、`/logs/`、
  `/*.pid`、`/start-rag-servers.ps1`、`/start-rag-servers.cmd`、`/gguf-info.ps1`、`/.ref-m3u8/`。
  即便如此仍请显式 `git add <file>`，不要 `git add -A`
- 新增运行期产物（pid / log / part 文件）时，先补 `.gitignore` 再提交

## Security

- 两个服务只监听 `127.0.0.1` 且无鉴权。若改 `--host 0.0.0.0` 对外暴露，必须同时加 `--api-key`
- 不要把 API Key 或机器相关的绝对路径写进代码；默认模型名/端口集中在 `start-rag-servers.ps1` 的
  `$services` 与 `src/service.rs` 的 `ServiceType` 中
- `config.json` 可能包含本机路径，已被忽略；不要提交
- 生成的 `.pid` 文件由脚本/管理器自动维护，不要手动编辑或删除正在运行服务的 PID 文件
