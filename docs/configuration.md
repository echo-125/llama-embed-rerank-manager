# 配置说明

GUI 的「设置」页可修改：

- 向量 / 重排模型端口（两个端口并排在一行；修改后重启对应模型生效）
- 模型目录（默认 exe 所在目录，可指向任意本地目录，一键恢复默认）
- `llama.exe` 路径（重新检测 / 手动选择 / 一键打开 Llama 下载页）
- 显卡信息（型号、显存总量、当前采样方式，可手动重新检测）
- 模型下载入口（模型目录 + Hugging Face GGUF 模型页）
- 行为偏好：关闭时最小化到托盘、退出时停止模型、导入后自动分配
- 主题（亮 / 暗）与语言（中文 / English）在顶部一行切换

## config.json

配置保存在模型目录下的 `config.json`，已加入 `.gitignore`：

```json
{
  "embedding_port": 9932,
  "rerank_port": 9933,
  "embedding_model": "Qwen3-Embedding-4B-Q6_K.gguf",
  "rerank_model": "bge-reranker-v2-m3-F16.gguf",
  "model_dir": null,
  "auto_assign_imported": true,
  "model_kinds": {},
  "llama_exe_path": null,
  "dark_mode": false,
  "language": "zh"
}
```

| 字段 | 说明 |
|---|---|
| `embedding_port` / `rerank_port` | 两个模型的监听端口，必须互不相同 |
| `embedding_model` / `rerank_model` | 所选模型文件名；`null` 表示尚未选择 |
| `model_dir` | 自定义模型目录；`null` 表示使用 exe 所在目录。目录不存在时自动回退到默认目录 |
| `auto_assign_imported` | 导入后是否按类型自动分配给对应服务（默认 `true`）；目标服务已有其他模型时仍会弹窗确认 |
| `model_kinds` | 手动修正的模型类型，`{"文件名": "embedding" \| "rerank" \| "other"}`，优先级高于自动识别 |
| `llama_exe_path` | `llama.exe` 路径；`null` 时自动探测默认安装路径 |
| `dark_mode` | 是否使用暗色主题 |
| `language` | `zh` 或 `en` |

配置写入是原子的（先写 `config.json.part` 再改名），断电或崩溃不会留下半个配置文件。

## 模型目录

- 默认：exe 所在目录（启动时工作目录里有 GGUF 则优先用工作目录）
- 设置页「模型目录」可改为任意本地目录，改完立即写入 `model_dir` 并重新扫描
- 「恢复默认」清空 `model_dir`，回到 exe 目录
- 切换目录不会中断正在运行的服务，但它们的模型文件来自旧目录，需要重新选择模型并重启

## 端口状态

- 停止状态下可直接在模型行编辑端口，与设置页共享草稿值
- 运行中的模型保存新端口后先记入待生效状态，重启/停止后生效
- 启动、重启、「全部启动」前会先应用草稿
