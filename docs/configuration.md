# 配置说明

GUI 的「设置」页可修改：

- 向量 / 重排模型端口（修改后重启对应模型生效；运行中的模型会显示待生效端口）
- `llama.exe` 路径（重新检测或手动选择）
- 主题（亮 / 暗）与语言（中文 / English）
- 关闭时最小化到托盘、退出时停止模型

## config.json

配置保存在模型目录下的 `config.json`，已加入 `.gitignore`：

```json
{
  "embedding_port": 9932,
  "rerank_port": 9933,
  "embedding_model": "Qwen3-Embedding-4B-Q6_K.gguf",
  "rerank_model": "bge-reranker-v2-m3-F16.gguf",
  "ignored_models": [],
  "llama_exe_path": null,
  "dark_mode": false,
  "language": "zh"
}
```

| 字段 | 说明 |
|---|---|
| `embedding_port` / `rerank_port` | 两个模型的监听端口，必须互不相同 |
| `embedding_model` / `rerank_model` | 所选模型文件名；`null` 表示尚未选择 |
| `ignored_models` | 「移除导入」后保留在磁盘、但不在列表中显示的模型文件名 |
| `llama_exe_path` | `llama.exe` 路径；`null` 时自动探测默认安装路径 |
| `dark_mode` | 是否使用暗色主题 |
| `language` | `zh` 或 `en` |

## 端口状态

- 停止状态下可直接在模型行编辑端口，与设置页共享草稿值
- 运行中的模型保存新端口后先记入待生效状态，重启/停止后生效
- 启动、重启、「全部启动」前会先应用草稿
