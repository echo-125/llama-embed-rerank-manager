# 安装与运行

## 依赖

- Windows 10/11 x64
- [Llama 桌面端](https://llama.app/)：提供 `llama.exe` 运行时，通常位于
  `%LOCALAPPDATA%\Microsoft\WindowsApps\llama.exe`
- 两个 GGUF 模型：一个向量模型、一个重排模型（模型体积较大，不随仓库提交）
- 从源码构建需要 Rust MSVC 工具链与 Windows SDK（`build.rs` 使用 `rc.exe` 嵌入 exe 图标）

## 运行 GUI

```bat
start-gui.cmd
```

或直接运行根目录的 `llama-rag-gui.exe`。首次加载 4B 向量模型约 20~30 秒。

## 从源码构建

```bat
cargo build --release
copy target\release\llama-rag-gui.exe .
```

- 产物：`target/release/llama-rag-gui.exe`
- 开发模式：`cargo run`（debug 保留控制台）
- 根目录的 `llama-rag-gui.exe` 是手动复制的发布副本；`start-gui.cmd` 启动的是它
- `assets/` 是图标资源，必须保留

## 放置模型

把 GGUF 文件放在程序目录（或启动时的工作目录）。应用会自动扫描该目录。

模型目录的解析顺序（不含任何本机固定路径）：

1. 启动时的工作目录里有 `.gguf` 文件 → 用工作目录
2. 否则用 exe 所在目录（发布形态：exe 与模型、`config.json` 同级）
3. 目录里暂时没有模型也没关系，可直接点「模型文件」页的「+ 导入」把模型放进来

模型类型按三级识别（手动修正 > GGUF 元数据 > 文件名），详见 [模型管理](models.md)：

- GGUF 元数据里有 `<arch>.embedding_length` → 向量模型
- 否则看文件名：含 `embed` / `bge-m3` / `gte-` / `e5-` 判为向量，含 `rerank` 判为重排
- 识别为 `Other` 的模型仍会列出，但不会出现在两个模型下拉中；也可以在模型文件页直接修正类型

## 下一步

- [模型管理](models.md)
- [API 与接入](api.md)
- [配置说明](configuration.md)
