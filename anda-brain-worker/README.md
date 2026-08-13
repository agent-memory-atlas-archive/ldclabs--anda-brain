# Anda Brain Worker

这是一个参考 Rust 版 `anda_brain`、基于 `@ldclabs/kip-do` 实现的简化版 Cloudflare Worker。它保留三条核心认知链路：

- Formation：把对话提炼成 KIP `UPSERT`，写入长期图谱记忆。
- Recall：把自然语言问题规划为只读 KIP 查询，再根据图谱证据生成答案。
- Maintenance：检查近期 Event / SleepTask，由模型规划合并、更新或归档。

每个 `space_id` 映射到一个独立的 SQLite Durable Object。KIP 图谱、原子写入、全文检索和模式胶囊由 `@ldclabs/kip-do` 提供；自然语言规划和答案合成使用 Workers AI。

## 与完整版的边界

这个实现面向小型 Agent、个人项目和边缘部署，不是 Rust 服务的完全移植：

| 能力 | Worker 版 |
| --- | --- |
| Formation / Recall / Maintenance | 保留，改为同步执行 |
| 每空间独立图谱 | SQLite Durable Object |
| KIP 查询与写入 | 保留 |
| JSON API | 保留核心入口 |
| CBOR / Markdown 协商 | 未实现 |
| 异步 Formation 队列与对话历史 | 未实现 |
| Wiki、MCP、BYOK、分级令牌 | 未实现 |
| 自动周期维护 | 未实现；由调用方或 Cron Trigger 调用 maintenance |

## 快速开始

从仓库根目录安装并启动：

```bash
pnpm install
cp anda-brain-worker/.dev.vars.example anda-brain-worker/.dev.vars
pnpm --filter @ldclabs/anda-brain-worker dev
```

本地调试可把 `.dev.vars` 中的 `BRAIN_API_KEY` 留空。部署前应配置密钥：

```bash
cd anda-brain-worker
pnpm wrangler secret put BRAIN_API_KEY
pnpm run deploy
```

`wrangler.jsonc` 默认使用 `@cf/meta/llama-3.3-70b-instruct-fp8-fast`。可通过 `AI_MODEL` 更换为另一个支持结构化 JSON 输出的 Workers AI 模型。

## API

当 `BRAIN_API_KEY` 非空时，所有 `/v1/{space_id}/*` 请求都必须带：

```text
Authorization: Bearer <BRAIN_API_KEY>
```

### Formation

```bash
curl http://localhost:8787/v1/alice/formation \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer replace-me' \
  -d '{
    "messages": [
      {"role": "user", "content": "以后回答尽量简洁，并优先给结论。"}
    ],
    "context": {"counterparty": "alice", "source": "chat-42"},
    "timestamp": "2026-08-12T12:00:00Z"
  }'
```

Formation 只允许模型执行 `UPSERT`。`UPDATE`、`DELETE`、`MERGE` 或任何查询命令都会在 Durable Object 内被拒绝。

### Recall

```bash
curl http://localhost:8787/v1/alice/recall_structured \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer replace-me' \
  -d '{
    "query": "我应该用什么风格回答 Alice？",
    "context": {"counterparty": "alice"}
  }'
```

召回计划只能执行 KQL / META。服务还会固定加入一个受限 `SEARCH CONCEPT ... LIMIT 8` 作为降级路径，因此模型生成了无效查询时仍可完成基本召回。

### Maintenance

```bash
curl http://localhost:8787/v1/alice/maintenance \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer replace-me' \
  -d '{"trigger":"on_demand","scope":"daydream"}'
```

Maintenance 可使用 `UPSERT`、单实体 `MERGE CONCEPT`，以及带 `LIMIT 20` 或更小上限的 `UPDATE`。为避免模型误删整批记忆，Maintenance 计划中的 `DELETE` 会被拒绝；需要原始管理操作时应使用受保护的 `execute_kip`。

### Probe 与直接 KIP

| Method | Path | 说明 |
| --- | --- | --- |
| `GET` | `/healthz` | 健康检查，不需要鉴权 |
| `GET` | `/v1/{space}/info` | 图谱节点数、命题数和初始化时间 |
| `GET` | `/v1/{space}/formation_status` | 同步模式状态 |
| `POST` | `/v1/{space}/probe` | 不调用 LLM 的轻量记忆搜索 |
| `POST` | `/v1/{space}/execute_kip_readonly` | 只允许 KQL / META |
| `POST` | `/v1/{space}/execute_kip` | 管理级原始 KIP，允许写入 |

直接 KIP 的请求体使用 `{ "command": "..." }` 或 `{ "commands": ["..."] }`。

## 中文分词

没有 `TOKENIZER` service binding 时，`kip-do` 使用内置 `SimpleTokenizer`。它适合测试和基本英文语料；中文会按单字切分。生产环境需要更好的多语言检索时，请部署 anda-db 中的 `cf-tokenizer`，并在 `wrangler.jsonc` 增加 service binding：

```jsonc
"services": [
  { "binding": "TOKENIZER", "service": "cf-tokenizer" }
]
```

写入与搜索必须使用同一个 tokenizer，不能在服务故障时临时切换分词策略。

## 安全边界

- API Key 为空会关闭鉴权，只适合本地开发。
- Formation 和 Recall 在 Worker 与 Durable Object 两层校验 KIP 类型。
- 模型生成的 Recall 查询必须显式设置 `LIMIT 20` 或更小；Maintenance 的 `UPDATE` 同样强制限制为 20，并拒绝 `DELETE`。
- 模型看到的对话、查询和图谱内容都被标记为数据，不能改变系统规则。
- 原始 `execute_kip` 是管理接口；不要把密钥交给不可信客户端。
- 一个空间对应一个 Durable Object，KIP 存储操作会在其中串行提交；AI 规划仍可能并行运行，高吞吐场景应拆分空间。

## 检查

```bash
pnpm --filter @ldclabs/anda-brain-worker typecheck
pnpm --filter @ldclabs/anda-brain-worker test
pnpm --filter @ldclabs/anda-brain-worker deploy:dry-run
```

测试运行在 workerd 中，会实际覆盖 SQLite Durable Object、KIP 写入、只读边界和召回链路。
