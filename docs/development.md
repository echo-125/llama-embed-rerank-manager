# 开发说明

## 项目结构

仓库根就是一个完整的 Rust 项目（`Cargo.toml` 与 `src/` 同级）。模型、发布 exe 与运行期文件在仓库外，位于上一级目录。

| 路径 | 说明 |
|---|---|
| `src/main.rs` | 应用状态、四个页签（模型运行 / 模型文件 / 运行日志 / 设置）、托盘事件与启动入口 |
| `src/modals/` | 弹窗：移除确认、同名冲突、自动分配确认、接口详情（各一个文件） |
| `src/ui.rs` | 跨页面复用的绘制小部件与纯函数（表格单元、状态色、文件名校验等） |
| `src/service.rs` | 向量 / 重排模型的进程、健康检查、PID、端口、`--alias`、日志队列 |
| `src/monitor.rs` | 内存 / 显存 / GPU 采样（优先 `nvidia-smi`，否则 Windows 性能计数器） |
| `src/pdh.rs` | Windows 性能计数器（PDH）的最小封装：枚举 GPU 实例、读取计数器数值 |
| `src/model_mgr.rs` | GGUF 扫描、导入 / 导出（预检、进度、取消、冲突）、类型判断与维度读取 |
| `src/config.rs` | `config.json` 读写 |
| `src/lang.rs` | 中英文文案 |
| `src/theme.rs` | 明暗主题与调色板 |
| `src/icon.rs` / `assets/` | llama.app 图标资源（窗口 / 托盘 / exe） |
| `build.rs` | 嵌入 Windows exe 图标 |
| `docs/` | 用户文档 |
| `start-gui.cmd` | 双击启动 GUI（启动上一级目录的 exe） |

上一级目录（不在版本控制内）：

| 路径 | 说明 |
|---|---|
| `*.gguf` | 模型文件；程序从 exe 所在目录扫描 |
| `llama-rag-gui.exe` | 发布副本，`start-gui.cmd` 启动它 |
| `config.json` | 运行时配置（原子写入） |
| `embedding.pid` / `rerank.pid` | 运行中模型的 PID 文件（PID + 实际端口） |

程序不会在模型目录下创建 `logs/` 等日志文件：运行日志只保存在内存环形队列里，由用户手动导出。


## 构建与检查

在仓库根（即本目录）执行：

```bat
cargo fmt
cargo test
cargo clippy
cargo build --release
copy target\release\llama-rag-gui.exe ..\
```

- 仅支持 Windows；其他平台构建失败是预期行为
- 发布前需要把 `target/release/llama-rag-gui.exe` 复制到**上一级目录**，`start-gui.cmd` 启动的就是那份副本
- 程序按 exe 所在目录扫描模型，所以 exe 必须留在上一级目录（与 `*.gguf`、`config.json` 同级）
- 上一级目录的 exe 被占用时，需先关闭 GUI 再复制

## 修改时的同步点

- 默认端口、模型名与 llama-server 参数：`src/service.rs` 的 `ServiceType`
- 接入模型名：`model_alias`、`start_service` 的 `--alias`、启动脚本
- 模型类型判断：`ModelKind::classify_with_metadata`、`config.model_kinds` 覆盖与模型下拉过滤
- 新增配置字段：`AppConfig`、README/docs、`config.json` 示例
- 导入 / 导出复制逻辑：`copy_with_progress`、冲突弹窗（`ImportConflict`）（不涉及外部脚本）

## 代码约定

- rustfmt 默认风格，4 空格缩进，edition 2024
- 标识符用英文；注释用中文；用户可见文案中英双语
- 颜色统一走 `theme::palette(dark)`；卡片与弹窗统一使用 `card_frame` / `section_title`
- 阻塞操作（进程拉起、目录扫描等）放在后台线程，避免阻塞 UI
- **公开项写文档注释**：每个 `pub fn` / `pub struct` / `pub enum` 至少一句说明「做什么、何时调用」；
  内部函数在有非显然约束时（unsafe、平台差异、时序）说明原因
- 状态栏与日志里的文案在生成时就固化成单一语言，切换语言后不会重译已显示的会话消息，
  这是预期行为；新增文案仍需双语
- 新增弹窗放 `src/modals/`，跨页面复用的绘制函数放 `src/ui.rs`，不要继续堆进 `main.rs`
