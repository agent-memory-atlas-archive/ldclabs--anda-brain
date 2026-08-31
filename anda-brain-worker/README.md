# Anda Brain Worker

这是一个参考 Rust 版 `anda_brain`、基于 `@ldclabs/kip-do` 实现的简化版 Cloudflare Worker，使用 **KIP 2.0**。它保留三条核心认知链路：

- Formation：把对话提炼成 KIP KML 事务，写入长期图谱记忆。
- Recall：把自然语言问题规划为只读 KIP 查询，再根据图谱证据生成答案。
- Maintenance：检查近期 Event / SleepTask / 最久未动的记忆，由模型规划合并、更新或归档。

每个 `space_id` 映射到一个独立的 SQLite Durable Object。KIP 图谱、原子事务和模式包由 `@ldclabs/kip-do` 提供；自然语言规划和答案合成使用 Workers AI。

## KIP 2.0 意味着什么

1.x 把含义、信念、证据、来源和模式塞在同一张图里；2.0 把它们分开，而其余区别都来自同一条：**一个 Proposition 存在，不等于它为真**。落到这个 Worker 上：

- 一条事实是「truth-neutral 的 Proposition」加上「携带某个 actor 立场、模式、置信度与 Evidence 的 Assertion」。更正是**新增一条 Assertion 并 SUPERSEDE**，绝不改写原有记录。
- 永远不要随时间衰减 Assertion 的 confidence。衰减的是 `MnemonicState.memory_strength`，那是可及性，不是真假。
- 元素 id 形如 `C-7`、`P-11`、`A-3`、`E-2`、`X-1`。
- **Schema 是受保护的控制状态：KML 不能声明类型。** 新词汇经由宿主进入，见下文。

## 依赖：本地的 kip-do 0.13

`@ldclabs/kip-do` 0.13 尚未发布，因此 `package.json` 用 `link:` 指向同级仓库：

```json
"@ldclabs/kip-do": "link:../../anda-db/ts/kip-do"
```

这与 Rust 侧的 `[patch.crates-io]` 同构：**发布前必须去掉**，而且缺少 `../../anda-db` 检出的克隆无法安装。`ts/kip-do` 的入口是 `dist/`，先构建：

```bash
cd ../../anda-db/ts/kip-do && pnpm install && pnpm run build
```

## 与完整版的边界

这个实现面向小型 Agent、个人项目和边缘部署，不是 Rust 服务的完全移植：

| 能力 | Worker 版 |
| --- | --- |
| Formation / Recall / Maintenance | 保留，同步执行 |
| 每空间独立图谱 | SQLite Durable Object |
| KIP 2.0 查询与写入 | 保留 |
| 每空间 Schema Package（新词汇） | 保留，见下节 |
| JSON API | 保留核心入口 |
| CBOR / Markdown 协商 | 未实现 |
| 异步 Formation 队列与对话历史 | 未实现 |
| Wiki、MCP、BYOK、分级令牌 | 未实现 |
| 自动周期维护 | 未实现；由调用方或 Cron Trigger 调用 maintenance |
| 全文检索（`SEARCH`） | 保留，keyword 模式，见「检索」一节 |
| 派生闭包（`LIST DEPENDENTS`） | 保留，`DEPTH` 上限 8；但 maintenance 一次只出 KML，走不了这条读 |
| 保留期（`SET RETENTION`） | 引擎未实现，因此没有到期清扫；Rust 服务两样都有 |
| 载荷清除（`PURGE PAYLOAD`） | 引擎已实现；maintenance 计划里和 `PURGE` 一样被拒 |
| 原子批（`execution.mode: "atomic"`） | 引擎未实现，请求会被拒绝而不是伪装成功 |

## 词汇表：新类型和新谓词

Cognitive Memory Profile 只自带 10 个类型和 3 个谓词（`prefers`、`caused_by`、`same_as`），够不上「Alice 在 Acme 工作」。KIP 2.0 又禁止 KML 声明类型，所以新词汇必须从宿主进入。

每个 Durable Object 维护一个 `kip://anda-brain/memory` 包，与 Profile 并行激活。模型在计划 JSON 里提出符号，宿主校验、限量、发版：

```json
{
  "types": ["Project"],
  "predicates": ["works_on"],
  "commands": ["MUTATE { … }"],
  "summary": "记录了 Alice 在做 Aurora 项目。"
}
```

- 类型必须 UpperCamelCase，谓词必须 snake_case；不合法的名字会出现在 `rejected` 里，不会被「顺手改成合法的」发布出去。
- 每个空间最多 512 个符号。
- **Profile 已有的符号不会被重新声明**：两个激活的包声明同一个本地名，会让每一次裸写 `{type: "Person"}` 都变成 `SchemaSymbolAmbiguous`。
- 只有 Formation 和 Maintenance 能扩展词汇；Recall 是只读的。
- `GET /v1/{space}/vocabulary` 可以查看当前词汇表。

Durable Object 在构造时重新激活「Profile + 本空间词汇包」这一整套。只激活 Profile 会**收窄**环境：锁里会丢掉本空间的包，已发布的本地名会突然解析不了，而且没有任何错误会说出原因。

## 检索

`SEARCH CONCEPT | PROPOSITION | EVIDENCE | COGNITION` 由 `kip-do` 基于 SQLite FTS5 + BM25 实现，索引在**写入同一个事务里**维护，所以答案里的 `index_seq` 和 `current_space_seq` 相等是构造保证，不是乐观估计（§66.5、§79）。

中文分词用 `Intl.Segmenter`（ICU 词典），在进程内同步完成，读写两条路径调同一个函数。1.x 那套外部 `cf-tokenizer` 服务和 `TOKENIZER` binding 都不再需要——2.0 的写入路径是同步的，事务里发不出 HTTP 请求。

```bash
curl http://localhost:8787/v1/alice/execute_kip_readonly \
  -H 'Content-Type: application/json' \
  -d '{"command": "SEARCH CONCEPT :term WITH TYPE \"Person\" LIMIT 10", "parameters": {"term": "深色模式"}}'
```

引擎没有的三样，写了会被拒：`MODE "semantic"` / `"hybrid"`（没有嵌入模型）、`AS OF SEQ`（索引不留自身历史）、`SEARCH ASSERTION` / `SEARCH ACTIVITY`（没有自由文本可索引，返回空会被读成「没有这条主张」）。

命中是**信封**：`{id, kind, score, element}`，类型和名字在 `element` 上而不是并列。`score` 是检索相关度，不是置信度；miss 也不是「不存在」。

`recall`、`probe` 和引用列表都由一条固定的 `SEARCH CONCEPT` 打底，跑在问模型之前——答案不该取决于规划器有没有想到去查。

## 提示词

三个模式提示词是 KIP 2.0 参考 Brain 策略（`anda-db/rs/anda_kip/brain/Brain*.md`），各自附一段本部署的契约（`# A. Anda Brain Worker deployment contract`），再拼上语法卡与 Cognitive Memory Profile。它们放在 `assets/`，由脚本内联：

```bash
pnpm run sync:assets      # 从 anda_kip 刷新全部 vendor 资产
pnpm run codegen:prompts  # assets/*.md -> src/assets.generated.ts（需提交）
```

`sync:assets`（仓库根的 `scripts/sync-kip-assets.mjs`，Rust 侧的 `anda_brain/assets/` 也归它管）逐字覆盖语法卡和 Profile，并且**按 `# A.` 标题切开**每份 `Brain*.md`：上半截参考策略从 `anda_kip` 重刷，下半截本部署契约原样留下。之前这三份写着「上游变了就手工 diff」，结果 KIP 2.0 `40e655f` 的 Watch、WorkingState、DerivationState、`MnemonicState.utility`、`LIST DEPENDENTS`、`PURGE PAYLOAD` 在参考策略里躺了好几天，五份副本一份都没有。

Rust 服务不需要 codegen：`anda_kip` 随协议发出语法卡和 Profile，运行时直接读。`kip-do` 两者都不发，所以这里保留副本——代价就是副本会漂移，`sync:assets` 是用来对抗这件事的。

一个模式提示词约 20k token，**24k 上下文的模型装不下**。`AI_MODEL` 默认 `@cf/meta/llama-4-scout-17b-16e-instruct`；生产环境可换成上下文更大、同样支持结构化 JSON 输出的模型。

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
    "timestamp": "2026-08-20T12:00:00Z"
  }'
```

`context.counterparty` 是 Concept 的 **key**（不可变身份），不是 name（可变标签）。

Formation 只能写认知：`CREATE CONCEPT`、`UPSERT CONCEPT`、`ENSURE PROPOSITION`、`CREATE EVIDENCE / ASSERTION / ACTIVITY`、`ASSERT`、`TRANSITION ACTIVITY`，以及用于更正的 `RETRACT` / `SUPERSEDE` / `CORRECT EVIDENCE`。`UPDATE`、`ARCHIVE`、`TOMBSTONE`、`PURGE`、`MERGE CONCEPT` 会在 Durable Object 内被拒绝。

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

召回计划只能执行 KQL / META，且每条必须带 `LIMIT 20` 或更小；无界、可变更或无法解析的命令会被静默丢弃。服务固定先跑一条上文的 grounding 查询，因此模型给出无效计划时仍能完成基本召回——它失败才算服务失败，模型规划的那几条失败只会记进 `diagnostics.planned_read_errors`。

### Maintenance

```bash
curl http://localhost:8787/v1/alice/maintenance \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer replace-me' \
  -d '{"trigger":"on_demand","scope":"daydream"}'
```

Maintenance 可以用 KML 的全部动作，但 **`PURGE` 被拒绝**（不可逆，且模型读自己的快照不是决定「让某物从未存在」的地方；需要时走管理级 `execute_kip`）。**任何带 `WHERE` 选择的子句都必须带 `LIMIT 20` 或更小**——`UPDATE ?e … WHERE {}` 和 `ARCHIVE ?e WHERE {}` 是同一个风险换了个动词。

请求参数里没有 `confidence_decay_factor`：2.0 禁止随时间衰减 Assertion 置信度。

### Probe 与直接 KIP

| Method | Path | 说明 |
| --- | --- | --- |
| `GET` | `/healthz` | 健康检查，不需要鉴权 |
| `GET` | `/v1/{space}/info` | 元素计数、Schema 环境版本和初始化时间 |
| `GET` | `/v1/{space}/vocabulary` | 本空间已发布的类型与谓词 |
| `GET` | `/v1/{space}/formation_status` | 同步模式状态 |
| `POST` | `/v1/{space}/probe` | 不调用 LLM 的轻量记忆查找 |
| `POST` | `/v1/{space}/execute_kip_readonly` | 只允许 KQL / META |
| `POST` | `/v1/{space}/execute_kip` | 管理级原始 KIP，允许写入 |

直接 KIP 的请求体与 Rust 服务一致：`{"command": "..."}` 或 `{"operations": [...]}`，二选一。`parameters` 会绑定进命令的 `:placeholder`（结构化绑定，不是字符串插值），单个 operation 自己的 `parameters` 覆盖共享的同名键。每个 operation 可以带自己的 `op_id`，会原样回显在对应结果上——这是批次答案与请求配对的唯一可靠方式。

`execution` 可选：`{"mode": "independent"}`（默认，各自独立提交）或 `{"mode": "sequence", "on_error": "stop"}`（一条失败后，其余答 `skipped` 而不执行）。`"atomic"` 被明确拒绝——本引擎没有跨 operation 的事务，把它当 sequence 跑就等于谎报了原子性。

```bash
curl http://localhost:8787/v1/alice/execute_kip_readonly \
  -H 'Content-Type: application/json' \
  -d '{
    "operations": [
      {"command": "FIND(?c.id, ?c.name) WHERE { ?c CONCEPT {type: \"Person\", key: :who} } LIMIT 5"}
    ],
    "parameters": {"who": "alice"}
  }'
```

响应是每个 operation 一项的数组。每项都带必填的 `status`（`succeeded` / `failed` / `skipped` / `no_effect`），读操作的行在 `result`，写操作的完整事务结果（`handles`、`changes`、治理决定）在 `extensions["kip-do/outcome"]`，失败在 `error`。判断成败请读 `status`：`no_effect` 既没有 `error` 也没有提交，把「没有错误」当成「写进去了」会漏掉这种情况。

## 安全边界

- API Key 为空会关闭鉴权，只适合本地开发。
- 只读、Formation、Maintenance 三道闸门都按命令**解析出来的语义**判断，而不是请求里的标签；Worker 和 Durable Object 两层都会校验。
- 模型看到的对话、查询和图谱内容都被标记为数据，不能改变系统规则。
- 原始 `execute_kip` 是管理接口；不要把密钥交给不可信客户端。
- 一个空间对应一个 Durable Object，KIP 存储操作会在其中串行提交；AI 规划仍可能并行运行，高吞吐场景应拆分空间。

## 检查

```bash
pnpm --filter @ldclabs/anda-brain-worker typecheck
pnpm --filter @ldclabs/anda-brain-worker test
pnpm --filter @ldclabs/anda-brain-worker deploy:dry-run
```

`pnpm --filter @ldclabs/anda-brain-worker check` 三条一起跑。测试运行在 workerd 中，会实际覆盖 SQLite Durable Object、KIP 2.0 写入、词汇表发布与驱逐后恢复、中英文 SEARCH 落地、只读边界和召回链路。
