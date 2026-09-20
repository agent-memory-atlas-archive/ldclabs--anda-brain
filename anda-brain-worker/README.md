# Anda Brain Worker

这是一个参考 Rust 版 `anda_brain`、基于 `@ldclabs/kip-do` 实现的简化版 Cloudflare Worker，使用 **KIP 2.0**。它保留三条核心认知链路：

- Formation：把对话提炼成 KIP KML 事务，写入长期图谱记忆。
- Recall：把自然语言问题规划为只读 KIP 查询，再根据图谱证据生成答案。
- Maintenance：检查近期 Event / SleepTask / 最久未动的记忆，由模型规划合并、更新或归档。

每个 `space_id` 映射到一个独立的 SQLite Durable Object。KIP 图谱、原子事务和模式包由 `@ldclabs/kip-do` 提供；自然语言规划和答案合成使用 Workers AI。

批量代谢跳过 SleepTask/Watch 运行记录，避免违反其版本守卫而让整批代谢失败。
失败通过 settlement.decay_error 报告，维护模型会收到实际 settlement。

## KIP 2.0 / CognitiveMemory 2.1 更新

Rust 使用已发布的 `anda_kip`、Cognitive Nexus 和 AndaDB 0.13；Worker 使用
已发布的 `@ldclabs/kip-do` 0.13。Skill 行为保存为不可变的
`SkillRevision`；Watch 进度和任务租约通过 Nexus 的受保护接口维护。旧的 family
成功率晋升规则已移除；未配置独立观察者、冻结试验和可重放评估时，程序候选保持未验证，
`skills.unsupported_reason` 明确报告该边界。现有 Brain API 保持可用；这两个适配器
**未声明支持**可选的五意图 Memory Interface 或 `memory_*` 能力包。详见
[同步说明](../docs/kip-v2-cognitive-sync.md)。

## KIP 2.0 意味着什么

1.x 把含义、信念、证据、来源和模式塞在同一张图里；2.0 把它们分开，而其余区别都来自同一条：**一个 Proposition 存在，不等于它为真**。落到这个 Worker 上：

- 一条事实是「truth-neutral 的 Proposition」加上「携带某个 actor 立场、模式、置信度与 Evidence 的 Assertion」。更正是**新增一条 Assertion 并 SUPERSEDE**，绝不改写原有记录。
- 永远不要随时间衰减 Assertion 的 confidence。衰减的是 `MnemonicState.memory_strength`，那是可及性，不是真假。
- 元素 id 形如 `C-7`、`P-11`、`A-3`、`E-2`、`X-1`。
- **Schema 是受保护的控制状态：KML 不能声明类型。** 新词汇经由宿主进入，见下文。

## 依赖：已发布的 kip-do 0.13

`@ldclabs/kip-do` 0.13 从 npm registry 安装，版本由仓库的
`pnpm-lock.yaml` 固定。无需同级 `anda-db` 检出或构建本地 `kip-do`：

```bash
pnpm install --frozen-lockfile
pnpm --filter @ldclabs/anda-brain-worker check
```

普通安装、测试和部署不需要同级 `anda-db` 检出。主动刷新 vendored KIP
提示资产时，`sync:assets` 默认使用同级源码；也可用 `ANDA_KIP_SOURCE` 指向
已下载的 `anda_kip` crate 目录，按发布版本同步。

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
| 确定性 settlement（代谢 / Nexus Watch 推进 / 更正发现） | 保留，见下节 |
| 全文检索（`SEARCH`） | 保留，keyword 模式，见「检索」一节 |
| 派生闭包（`LIST DEPENDENTS`） | 保留，`DEPTH` 上限 8；runtime 在 settlement 里替模型走：每条新 superseded 的 Assertion 带着它的 dependents 进 `assessment.revised_roots` |
| 保留期（`SET RETENTION`） | 引擎已实现，maintenance 可以写；但没有到期清扫，Rust 服务两样都有 |
| 载荷清除（`PURGE PAYLOAD`） | 引擎已实现；maintenance 计划里和 `PURGE` 一样被拒 |
| 原子批（`execution.mode: "atomic"`） | 引擎未实现（`atomic_batch` 能力为 false），请求按 §75.3 被拒绝而不是降级成 sequence |

## 确定性 settlement 与受保护操作

每次 maintenance 在模型调用前执行记忆强度代谢、更正发现和结构化 Watch 推进。
代谢只修改 `MnemonicState.memory_strength`，默认每周乘以 0.95、下限 0.3，绝不衰减
Assertion confidence。读取不强化记忆。
本次 `parameters.memory_strength_decay_factor` 会覆盖默认因子，并实际作用于确定性代谢；例如 `1` 保持强度不变。
更正扫描通过 `(space_seq, assertion_id)` 继续分页，同一事务超过 20 条也不会丢弃尾部。
响应中的 `settlement.corrections.incomplete` 表示尚未证明 backlog 已读完；`cursor_after_id`
在需要从事务内部继续时出现。发现进度不表示模型已处理这些更正。

Watch 先以 `disarmed` 创建，再通过宿主 `armWatch` 领取新 generation 与授权观察依据。
结算调用 `advanceWatch`，提交整体版本与 generation，让 Nexus 检查完整性水位和权限变化。
只有完整授权覆盖越过截止时间，silence Watch 才能触发；等到的变化已经发生时，截止后
状态为 `expired`，兼容响应字段 `disarmed` 统计该数量。引擎返回状态、coverage、receipt，
不会凭空生成 `watch_fire` Activity。文本条件和混合 text/selector 条件保持 deferred；
本适配器没有语义 evaluator。模型完成一轮不再被记录成 `consumed_seq`。旧 Watch 缺少
WatchState 时，先审查观察缺口，再显式重新 arm。

Skill 保持稳定身份，行为放入不可变 SkillRevision，current_revision/revision_of 双向引用
可在同一 MUTATE 创建。旧的 family 成功率规则已删除：family 仅用于寻找可比较样本，
不能自动选定基线。未配置独立观察者、冻结 TrialRecord、重放材料和受保护评估策略时，
程序候选保持未验证，`settlement.skills.unsupported_reason` 说明未运行评估；旧计数
字段仍为零。模型不能写学习记录、WatchState 或 LeaseState。

模型计划可选两个宿主字段（普通 HTTP 请求形状不变）：

```json
{
  "types": [], "predicates": [], "commands": [], "summary": "领取维护任务，下一轮处理。",
  "digests": {"digest_revision": {"task_family":"deploy", "procedure":"verify first"}},
  "runtime": [{"operation":"lease_task", "target_ref":"C-12", "expected_version":3}]
}
```

`digests` 至多四项、总量至多 64 KiB，键以 `digest_` 开头；宿主以 kip-jcs-safe-v1
规范化 JSON 并计算 SHA-256，作为 `:digest_revision` 等参数传给 KML。revision 摘要覆盖
全部 attributes，排除 behavior_digest。Nexus 会复核摘要与实际行为一致。

`runtime` 至多四项，支持 `arm_watch`、`lease_task`，仅 Maintenance 可用。顺序为：
校验整份计划 → 发布合法词汇 → 执行 runtime → sequence/stop 执行 KML。租期由宿主固定
为五分钟；身份来自认证 Session。每次操作后需要重新读取整体版本，这个单轮模型应在
下一次 snapshot 中读取结果，再把任务终态与输出放进同一个有 CAS 的 MUTATE。
runtime 结果通过 maintenance 响应的 `runtime` 数组返回；后续失败不会抹掉先前操作的
receipt，错误数据保留已执行的结果。整个计划不是事务，不要自动重放已成功的前缀。

使用 CognitiveMemory 2.0 精确 `schema_ref` 写入的旧 Watch/SleepTask 不能原地获得
2.1 的 WatchState/LeaseState。runtime 会在调用受保护操作前返回迁移说明：创建 2.1
替代记录，复制并核验必要的语义字段和结构引用，再归档旧记录；不要复用按 lineage
唯一的旧 key。

`assessment.revised_roots` 仍提供有界依赖遍历；缺页或不可访问的闭包显式标记 incomplete。
Nexus 的虚拟 dependency_validity 决定派生内容能否使用，存储的 review 不能覆盖它。
快照里的 space_seq 也不等于 WorkingState 的真实计算依据；缺少实际版本/basis 时推迟刷新。

## 词汇表：新类型和新谓词

Cognitive Memory Profile 提供标准记忆类型和谓词，业务领域仍可能需要新符号。KIP 2.0 又禁止 KML 声明类型，所以新词汇必须从宿主进入。

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

三个模式提示词是 KIP 2.0 参考 Brain 策略（`anda-db/rs/anda_kip/brain/Brain*.md`），各自附一段本部署的契约（`# A. Anda Brain Worker deployment contract`），再拼上对应角色卡与 Cognitive Memory Profile。它们放在 `assets/`，由脚本内联：

```bash
pnpm run sync:assets      # 从 anda_kip 刷新全部 vendor 资产
pnpm run codegen:prompts  # assets/*.md -> src/assets.generated.ts（需提交）
```

`sync:assets`（仓库根的 `scripts/sync-kip-assets.mjs`，Rust 侧的 `anda_brain/assets/` 也归它管）逐字覆盖语法卡和 Profile，并且**按 `# A.` 标题切开**每份 `Brain*.md`：上半截参考策略从 `anda_kip` 重刷，下半截本部署契约原样留下。之前这三份写着「上游变了就手工 diff」，结果 KIP 2.0 `40e655f` 的 Watch、WorkingState、DerivationState、`MnemonicState.utility`、`LIST DEPENDENTS`、`PURGE PAYLOAD` 在参考策略里躺了好几天，五份副本一份都没有。

Rust 服务不需要 codegen：`anda_kip` 随协议发出语法卡和 Profile，运行时直接读。`kip-do` 两者都不发，所以这里保留副本——代价就是副本会漂移，`sync:assets` 是用来对抗这件事的。

提示词包含角色策略和完整 ontology；实际 token 成本应按部署模型的 tokenizer 测量。`AI_MODEL` 默认 `@cf/meta/llama-4-scout-17b-16e-instruct`；生产环境可换成上下文更大、同样支持结构化 JSON 输出的模型。

### 内嵌参考查阅

提示词中的 Markdown 相对链接只表示出处，不是运行时可打开的文件。Worker
内嵌 21 份版本锁定的协议参考，包括规范、语法、角色卡、Profile、EBNF 和 JSON
Schema；部署后读取它们无需源码目录或网络。参考说明不代表 Worker 支持协议的全部能力。

Worker 使用结构化 JSON 中的可选 `references` 字段完成只读查阅，不要求模型支持
原生工具调用。例如 Recall 规划阶段可以先返回：

```json
{"commands": [], "references": [{"document": "syntax", "section": "kql", "offset": 0}]}
```

`document="index"` 列出文档 ID；`section="index"` 列出精确章节名，`section=null`
读取全文；语法卡另有 `kql/kml/meta/envelope` 别名。正文每页最多 8 KiB UTF-8 字节，
使用返回的 `next_offset` 续读同一文档/章节。未知 ID、章节和非法偏移会返回错误。
每阶段最多 3 轮查阅、8 页，每轮最多 4 页；达到限制后必须提交最终结果。
不查阅时保留原来的单次调用路径，查阅产生的所有模型调用计入 `usage`。

Formation、Maintenance、Recall 规划及回答阶段均支持查阅。查阅响应中其他必填字段
必须使用空占位值：计划的 `types/predicates/commands` 为 `[]`、`summary` 为 `""`；
回答的 `answer=""`、`found=false`、`uncertainty=1`。禁止夹带执行计划、词汇声明、
digest 或运行时动作。最终结果省略 `references` 或使用 `[]`，才会进入原有校验及执行流程。
查阅不会读取图谱、更新快照、扩展权限，也不构成记忆证据或变更覆盖。

`@ldclabs/kip-do` 和参考资源锁定为 `0.13.1`。从仓库根目录刷新资源：

```bash
ANDA_KIP_SOURCE=/path/to/published/anda_kip-0.13.1 node scripts/sync-kip-reference.mjs --worker
pnpm --filter @ldclabs/anda-brain-worker run codegen:prompts
```

`assets/kip-reference.json` 保存补充正文与 SHA-256 清单，已有角色卡、语法和 Profile
复用原来的资产。codegen 校验版本和哈希后生成 `src/references.generated.ts`。
`pnpm check` 同时检查生成文件漂移、类型、测试和部署打包；上游参考正文不得手改。

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
    "timestamp": "2026-08-20T12:00:00.000Z"
  }'
```

Formation / Maintenance 的 `timestamp` 接受带时区偏移及不同小数精度的 RFC 3339，
宿主统一转为 `YYYY-MM-DDTHH:mm:ss.SSSZ`。无法解析或缺省时使用本次请求的接收时间，
不会因为时间戳格式而拒绝消息；Formation 规划上下文保留原始时间戳文本。

`context.counterparty` 是 Concept 的 **key**（不可变身份），不是 name（可变标签）。宿主会在规划前确保该 Person 存在，并保留已有显示名称。

`context.source` 是线程/渠道来源，不是消息去重键。Evidence 身份由完整输入、上下文和时间戳的摘要确定；
同一 source 的后续消息不会复用旧消息的 Evidence。要重试同一观察，应保持消息、上下文及显式 `timestamp`
不变且可解析；省略或无法解析 timestamp 时，每个请求获得新的观察时间。此规则只保证 Evidence 的身份，不表示整份模型写入计划
具备请求级 exactly-once 语义。

Formation 只能写认知：`CREATE CONCEPT`、`UPSERT CONCEPT`、`ENSURE PROPOSITION`、`CREATE EVIDENCE / ASSERTION / ACTIVITY`、`ASSERT`，以及用于更正和自身 Activity 的 `TRANSITION`——状态限于 `retracted` / `superseded` / `corrected` / `running` / `completed` / `failed` / `cancelled`。`TRANSITION ... TO "archived"`、`TO "tombstoned"` 以及 `UPDATE`、`PURGE`、`MERGE CONCEPT` 会在 Durable Object 内被拒绝。状态必须写成字面量：闸门读不到的参数化状态一律拒绝，否则「六条语句合并成一条 `TRANSITION`」就等于给 Formation 开了一条以绑定值 tombstone 的路。

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

Maintenance 可以使用受限的维护 KML，学习和运行时 Facet 由宿主保护； **`PURGE` 和 `PURGE PAYLOAD` 被拒绝**（不可逆，且模型读自己的快照不是决定「让某物从未存在」的地方；需要时走管理级 `execute_kip`）。**任何带 `WHERE` 选择的子句都必须带 `LIMIT 20` 或更小**——`UPDATE ?e … WHERE {}` 和 `TRANSITION ?e TO "archived" WHERE {}` 是同一个风险换了个动词。`MERGE CONCEPT` 语法上没有 `LIMIT` 位置，所以对它的要求是 `WHERE` 必须精确指出源和目标，一次合并一对。

`SET RETENTION` 引擎已实现，maintenance 可以写保留期类别和 `expires_at`；被拒的只有 `legal_hold` 这一个成员，两个方向都拒——法务保留会挡住所有人的擦除，不是模型读一张快照就该做的决定。到期清扫本身 Worker 没有，写下的 `expires_at` 要靠调用方或 Rust 服务去执行。

请求参数：`memory_strength_decay_factor`、`stale_event_threshold_days`、`unconsolidated_max_backlog`（兼容旧名 `unsorted_max_backlog`）、`orphan_max_count`——与 Rust 服务的 `MaintenanceParameters` 逐字段对齐。没有 `confidence_decay_factor`：2.0 禁止随时间衰减 Assertion 置信度。

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
  只读那条还有第三层：Durable Object 把引擎自己的 `readonly` 标志一并传下去，由语句路径内部再判一次 `parseKip` 的结论。闸门是给出答案的那个——它在任何东西跑之前就点名拒绝的命令；引擎标志是底板，上面漏了只会赔上一个 `ReadonlyViolation` 的 operation，而不是一次已提交的写入。
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
