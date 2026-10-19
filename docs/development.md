# 开发说明

## 项目结构

仓库根就是一个完整的 Rust 项目（`Cargo.toml` 与 `src/` 同级）。模型、发布 exe 与运行期文件在仓库外，位于上一级目录。

| 路径 | 说明 |
|---|---|
| `src/main.rs` | GUI 主体：模型运行 / 模型文件 / 运行日志 / 设置，主题 / 语言、弹窗、托盘事件 |
| `src/service.rs` | 向量 / 重排模型的进程、健康检查、PID、端口、`--alias` |
| `src/monitor.rs` | 内存 / 显存 / GPU 采样 |
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
| `config.json` | 运行时配置 |
| `logs/` | 运行日志 |


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
