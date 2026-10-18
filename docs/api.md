# API 与接入

两个模型都基于 llama.cpp（llama-server），只监听 `127.0.0.1`，无鉴权。

## 接入模型名

客户端 `model` 字段填所选 GGUF 文件名去掉 `.gguf`：

- 向量模型默认：`Qwen3-Embedding-4B-Q6_K`
- 重排模型默认：`bge-reranker-v2-m3-F16`

应用启动 llama-server 时通过 `--alias` 使用同一名称，换模型后自动变化。API 弹窗中显示的「模型」就是该名称。

## Embedding

`POST http://127.0.0.1:9932/v1/embeddings`

```json
{
  "model": "Qwen3-Embedding-4B-Q6_K",
  "input": ["你好，世界", "hello world"]
}
```

- OpenAI 兼容；`input` 可为字符串或字符串数组（建议一次 ≤ 32 条）
- 输出维度由模型决定（默认模型为 2560 维）；单位向量，点积即余弦相似度
- 上下文 `-c 4096`，超长文本会被截断
- 响应字段：`model`、`usage`、`data[].embedding`

## Rerank

`POST http://127.0.0.1:9933/v1/rerank`

```json
{
  "model": "bge-reranker-v2-m3-F16",
  "query": "什么是向量数据库",
  "documents": ["向量数据库用于相似度检索", "今天天气不错"],
  "top_n": 2
}
```

- Jina / Cohere 风格
- `results` 已按 `relevance_score` 降序，`index` 对应 `documents` 下标
- 分数是原始 logit，不是概率；需要 0~1 时自行做 sigmoid
- 建议使用相对排序，不要用绝对阈值判断相关性

## 快速验证

```powershell
Invoke-RestMethod http://127.0.0.1:9932/health
Invoke-RestMethod http://127.0.0.1:9933/health

$body = @{ input = @('你好，世界'); model = 'Qwen3-Embedding-4B-Q6_K' } | ConvertTo-Json
Invoke-RestMethod -Method Post http://127.0.0.1:9932/v1/embeddings `
  -ContentType 'application/json' -Body $body
```

## 接入 RAG / Agent

- **Embedding**：Base URL `http://127.0.0.1:9932/v1`；模型名填上面的接入模型名；API Key 随意
- **Rerank**：URL `http://127.0.0.1:9933/v1/rerank`；模型名同理
- 常见 GUI 工具（Dify / FastGPT / AnythingLLM / Cherry Studio / Open WebUI）：选择 OpenAI 兼容供应商；向量维度填模型实际维度（默认 `2560`）
- **Docker 场景**：容器内的 `127.0.0.1` 不是宿主机；可用 `host.docker.internal:9932`。如需对外监听，必须把 `--host` 改为 `0.0.0.0` 并加 `--api-key`（见 `src/service.rs`）

## 安全

默认只监听本机且无鉴权。不要把端口直接暴露到局域网或公网。
