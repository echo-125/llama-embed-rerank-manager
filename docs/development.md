# 开发说明

## 项目结构

| 路径 | 说明 |
|---|---|
| `src/main.rs` | GUI 主体：模型运行 / 模型文件 / 运行日志 / 设置，主题 / 语言、弹窗、托盘事件 |
| `src/service.rs` | 向量 / 重排模型的进程、健康检查、PID、端口、`--alias` |
| `src/monitor.rs` | 内存 / 显存 / GPU 采样 |
| `src/model_mgr.rs` | GGUF 扫描、导入、类型判断与维度读取 |
| `src/config.rs` | `config.json` 读写 |
| `src/lang.rs` | 中英文文案 |
| `src/theme.rs` | 明暗主题与调色板 |
| `src/icon.rs` / `assets/` | llama.app 图标资源（窗口 / 托盘 / exe） |
| `build.rs` | 嵌入 Windows exe 图标 |
| `docs/` | 用户文档 |
| `start-gui.cmd` | 双击启动 GUI |

本地辅助脚本（`start-rag-servers.ps1` / `.cmd`、`gguf-info.ps1`、`logs/`、`config.json`）已被 `.gitignore` 忽略，不随仓库发布。

## 构建与检查

```bat
cargo fmt
cargo test
cargo clippy
cargo build --release
copy target\release\llama-rag-gui.exe .
```

- 仅支持 Windows；其他平台构建失败是预期行为
- 发布前需要把 `target/release/llama-rag-gui.exe` 复制到根目录，`start-gui.cmd` 启动的是根目录副本
- 根目录 exe 被占用时，需先关闭 GUI 再复制

## 修改时的同步点

- 默认端口、模型名与 llama-server 参数：`src/service.rs` 与本地 `start-rag-servers.ps1`
- 接入模型名：`model_alias`、`start_service` 的 `--alias`、启动脚本
- 模型类型判断：`ModelKind::classify` 与模型下拉过滤
- 新增配置字段：`AppConfig`、README/docs、`config.json` 示例

## 代码约定

- rustfmt 默认风格，4 空格缩进，edition 2024
- 标识符用英文；注释用中文；用户可见文案中英双语
- 颜色统一走 `theme::palette(dark)`；卡片与弹窗统一使用 `card_frame` / `section_title`
- 阻塞操作（进程拉起、目录扫描等）放在后台线程，避免阻塞 UI
