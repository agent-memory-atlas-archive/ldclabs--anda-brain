# 上下文信任运行时

**[English](TRUST_RUNTIME.md) | [中文](TRUST_RUNTIME_cn.md)**

信任运行时可生成受控的来源校准建议，并通过 Nexus 0.13.1 的原子 trust/提案/治理审计接口
应用。精简库也可使用，默认关闭；启动配置绝不自动授予 `manage_trust`。它调整的是
限定域内的来源权重，不是 Assertion confidence、事实真假、utility、Skill standing
或执行权限。旧 `source_reliability` 更正计数继续是诊断，不用于校准。

## 已支持的方法

首版使用 `binary-fact-accuracy-v1`：由独立核验者判断实际来源 Assertion 的事实陈述
是否正确，严格限制为一个谓词和一个上下文。两个选择器均必填，Assertion 必须明确
带有该上下文。来源 semantic actor 必须是规范 Concept ID，原始事务作者必须与
核验者不同。假设、预测、不确定声明、单独存在的 Proposition、其他谓词/上下文及
不在声明有效时段内的核验均拒绝。历史 supersession 不改写来源曾经作出的声明。

已注册 instrument 负责验证声明归属、事实准确性、环境稳定性和时间，原始比对材料
放在 `material`。只有 `verified_fact` 和明确的 `correct` 才产生样本。执行失败、缺少
前提、环境变化和未知原因单独记录为排除项；附带的布尔值或行动得分不能改变这一点。
宿主不会把 Assertion confidence 解释成概率，本方法不实现预测概率校准。

`root_key` 标识 instrument 的相关证据组，副本、摘要沿用原组。运行时还按来源的
逻辑声明（actor/Proposition/stance/context/valid-time）去重，因此更换事件 ID、
Evidence ID 或重新命名根，都不能把同一事实的反复核验变成独立样本。已用于应用的
根/声明不能在下一轮再次累加；更正后的新 ID 也不自动成为新独立事实。

只有显式配置参数，才按原生创建序号选择最先满足条件的 `minimum_samples` 个独立、
未消费样本。记二元准确率为 `a`、样本数为 `n`，Hoeffding 半径为
`sqrt(ln(2/alpha)/(2*n))`，区间裁剪至 `[0,1]`，置信度为 `1-alpha`。
拟变更值为 `clamp(old + clip(gain*(a-old), -step_cap, step_cap), 0, 1)`。
旧值从当前原生上下文 lookup 读取，存在歧义时拒绝。没有默认统计参数；参数缺失、
样本不足或核验接收/材料未决时，不产生可应用提案，也不把缺失观察猜成成功或失败。

独立性及采样假设需要部署验证。签名、配置摘要或“已审查”字段不证明实际独立性与
效果。启用自动变更前，仍需在 MIB 独立验证 instrument、样本总体、参数和错误率。

## 配置与授权

加载 Space 前，通过 `BRAIN_RUNTIME_CONFIG` 或可信 `SpaceRuntimeBindings` 安装
每 Space 的 `trust`。示例见 [trust.runtime.example.json](trust.runtime.example.json)。模板参数和校准均为 null，开关全部关闭。替换 Space ID、实际
context 引用（`C-1` 仅占位）、完整谓词 URI、instrument/environment 摘要和服务身份。
模型工具、普通 HTTP 请求和 MCP 工具不能安装或修改这些绑定。

| 配置项 | 含义 |
| --- | --- |
| `proposer_principal` | 读取和存放可审查工件的宿主身份，与核验者、治理者、普通调用者分开 |
| `observer` | 已注册的核验者、配置摘要及独立控制域 |
| `governor_principal` | 显式治理主体，不能由 semantic actor 推导 |
| `parameters` | `minimum_samples` 为 1–32，`alpha` 在 `(0,1)`，`gain` 和 `step_cap` 在 `(0,1]`；全部显式提供 |
| `calibration` | 有界非空材料；审查者与 proposer/verifier 分开；准确 `contract_digest` 和明确批准 |
| `automatic` | 自动发现与生成建议 |
| `apply` | 允许显式审查后的应用，需校准及治理身份 |
| `automatic_apply` | 在前两个开关开启时，允许已批准方法作为自动审查策略 |

`bootstrap:true` 可为 proposer 配置 `read/read_history/read_governance_history/create/derive`，
为 verifier 配置 `read/read_history/create/derive/record_outcome`，但不会
创建治理授权或授予 `manage_trust`。必须由现有治理管理员单独授权 governor 的
`manage_trust` 及必要材料读取权限。原生治理审计不要求 governor 拥有普通 Create。
重启不会恢复已撤销授权。自动路径只使用部署明确安装的宿主身份，普通观察请求体
不能提供治理身份或批准。

## 宿主接口及后果发现

`Space::trust()` 返回可信运行时。以下为 Rust 宿主 API，不是新增的 HTTP/MCP 写入端点：

| API | 行为 |
| --- | --- |
| `record_verification(auth, TrustVerificationInput)` | 新鲜认证的独立核验接收，返回原生 Evidence ID |
| `enqueue(evidence_ref)` | 重验并接收本合同下已有核验记录 |
| `propose(actor_ref)` | 显式生成冻结建议，包含排除项和不确定性 |
| `proposals(after, limit)` | 当前提案分页，每页 1–32 项 |
| `proposal(id)` / `receipt(id)` | 受控历史提案、已记录应用回执 |
| `apply(auth, id, reason)` | 当前治理主体、真实权限、准确提案和原生 CAS |
| `abandon(auth, id, reason)` | 关闭确定未提交的审查，不能借此删除已应用变更 |
| `propose_restore(auth, previous_id, reason, evidence_refs)` | 生成待单独应用的精确限定域回退建议 |
| `status()` | 配置、开关、校准、治理权限及恢复原因 |

`TrustVerificationInput` 包含 `event_key`、`root_key`、`assertion_ref`、`assessed_at`、
`cause`、可选 `correct` 及非空对象 `material`，完整输入最多 16 KiB。AuthContext 必须来自可信认证边界，不能
从持久 JSON 或 `asserted_by` 重建。相同事件重试复用原生身份，冲突内容拒绝。接收中断
且材料尚未提交时，核验者需带新鲜认证重试相同输入；其他事实不能替未决记录补齐证明。

已注册后果观察者可用 measurement outcome 的
`payload:{"trust_verification_ref":"E-…"}` 公告既有核验引用。
Outcome 仍遵循原本的真实 Attempt/Decision 与认证要求。trust 消费者仅用它发现记录，
重新读取独立核验，不把行动成功、失败或 magnitude 当可靠度分数。更正投递后果不等于
宣布独立事实核验错误；实际事实 Evidence 错误时，应使用原生 Evidence 更正协议。

## 原子性、恢复与边界

运行时保留全局 actor weights、默认值及所有其他规则，只变更准确的
actor/predicate/context 选择器。原生接口将审查后的提案/方法关联、trust 控制版本及
治理审计放入同一 redo 事务；Evidence 内容不可改写，active/corrected 状态会在该事务
内部再次核对。模型和 semantic actor 不因此获得任何控制面权限。

原生写入前，私有 CAS 日志保存定位信息或审查后的提交意图，只含引用、摘要、计数和
状态。核验正文存入原生 Artifact，绑定核验 Evidence 自身及其所有来源；来源或核验
记录被擦除时，正文也不可继续读取。提案/审查工件继承材料来源限制。调用方取消等待
不会取消已接收写入，关闭时会排空。

丢失确认时，核对准确的历史原生 trust 版本、提案和作者。最多使用 64 次公开历史序号
读取，不依赖内部幂等键格式或宿主全表扫描。后续设置、更正不把已提交变更变成未提交。
版本冲突不会静默重算或覆盖其他设置，须显式重新提案/审查，或放弃确定未提交的审查。
回退追加新版本与理由，要求当前 Evidence，仅恢复原限定规则或原本无该规则的状态；
后续已改变的限定域设置不能被盲目覆盖，回退也不冒充新增独立样本。

原生 trust 版本变化会让旧 ProjectionBasis、Watch 和依赖重验。Brain 也会清理诊断
miss cache 并标脏已注册注意力工作，不会自动 re-arm 掩盖观察空窗。已应用控制属于
治理历史；后续数据争议需要新的合格审查/回退，不改写历史或自动删除旧设置。

目录最多 4,096 个目标，每目标 64 个待处理核验。提案材料最多 256 KiB / 256 个来源，
已选 Evidence 最多 32 条；复用的私有 CAS 日志单对象上限 8 MiB。一轮后台工作最多
读取 8 个后果索引槽并处理一个目标，独立于 Watch、Formation、Maintenance，每 Space
单飞，每存储 shard 一个活动所有者。存储失败保留游标/意图，明确不属于本合同的公告
留下排除记录。HTTP/MCP 状态新增 trust 开关及恢复原因，不暴露原始样本或管理工具。
