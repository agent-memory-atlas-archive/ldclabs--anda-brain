# 运行时接入

**[English](RUNTIME.md) | [中文](RUNTIME_cn.md)**

Rust 服务负责持久化注意力调度及显式配置的记忆消费者。已注册 Space 默认启用
结构化 Watch 调度；动作交付、语义求值、学习、utility 与 trust 分别配置绑定和开关。
模型文本或普通请求不能安装执行器、观察者、求值器或治理权限。

| 文档 | 用途 |
| --- | --- |
| [API](API_cn.md#有身份的运行时待办与后果) | HTTP/MCP 认证、载荷与状态 |
| [学习](LEARNING_RUNTIME_cn.md) | 配对试验、业务适配、归档与复审 |
| [记忆效用](UTILITY_RUNTIME_cn.md) | 交付收据、贡献归因与校准排序 |
| [语义 Watch](SEMANTIC_WATCH_RUNTIME_cn.md) | 模型合同、覆盖、预算与恢复 |
| [上下文信任](TRUST_RUNTIME_cn.md) | 独立核验、限定域提案与治理 |

## 启动装配

HTTP 与 MCP stdio 共用启动装配，使用 `BRAIN_RUNTIME_CONFIG=/absolute/path/runtime.json` 或 `--runtime-config <path>`读取版本化 JSON。
Space 名称沿用 AndaDB 的 1–64 字符小写字母、数字及下划线规则。
在共享 AppState、加载 Space 前安装。首个编译内适配器 `attention_inbox_v1` 会经
原生 act/Attempt/fence 准入实际写入持久化 inbox；未知适配器、无效预算、缺失秘密
引用或未映射的接收者会明确报错。投递成功不代表人已阅读，也不自动生成 Outcome。

示例见 [runtime.example.json](runtime.example.json)。部署时替换为真实的 CWT subject
及观察方法配置摘要。subject 由 `ED25519_PUBKEYS` 验签，不是 semantic actor。
Space token 可用 `space_token_env` 从指定环境变量解析已签发的 token，映射只保留
其摘要；显示名称相同的新 token 不会继承旧映射。动作/观测请求不能安装凭据、工具
URL 或 shell 命令。

`bootstrap:true` 是显式的原生主体/权限装配，控制器和观察者必须分离。持久标记保证
重启不重新授予已撤销权限；中断且无法确认的装配需要人工审查。设为 false 时由可信
宿主自行配置 Nexus 治理。权限变化可能使旧 Watch basis 失效，应先审查旧工作再迁移
pins/重新布防，不会自动 re-arm。

适配器可使用显式 ContextRequest；普通通知也可选择一个实际存在的 Proposition
作为读取坐标，绝不据此认定它为真。动作 gate 继续读取约束/BELIEF 并执行预算，空图无坐标
时保持 blocked。可配置固定澄清问题；已有有效回答时不再发送尚未投递的问题。

嵌入式宿主可在共享/加载前调用 `AppState::with_memory_runtime_bindings(MemoryRuntimeBindings)` 复用动作回调，不能同时安装全局及逐 Space 执行器。
`Space::memory_runtime()` 提供可信接口，`consequences()` 提供独立后果 Rust 接收层。
运行并等待后台生命周期关闭；隔离 fork 不继承活动绑定，停机先排空观测写入，再关闭
learning、attention 和 AndaDB。

## 认证

待办/状态必须有真实 read/* 凭据及显式映射，不接受公共 Space 匿名兜底或本地关闭认证
时伪造的 CWT 身份。回答需要 write/*、当前可见性及正确接收者。HTTP 后果需要验签的
观察者 CWT、匹配合同及当前原生 record_outcome 权限；普通 write/* 或 Space token
不能冒充观察者。未配置验签器时该 HTTP 能力关闭，真实 Rust 宿主认证入口仍可使用。

## 宿主接入

HTTP 服务和 MCP stdio 已接入后台生命周期。嵌入式宿主需运行
`AppState::start_background_tasks` 并在停机时取消、等待它结束。自定义策略在共享宿主、
加载 Space 前用 `with_attention_policy` 安装。实验/影子宿主不会因此自动运行。

| 可信 Rust API | 用途 |
| --- | --- |
| `AppState::attention_tick()` | 手动执行与后台共用的一轮有界调度 |
| `Space::attention()` | 获取本 Space 的宿主运行时 |
| `AttentionRuntime::arm_watch(id, version)` | 先注册和标脏，再原生布防 |
| `register_work()` | 直接使用 Nexus 的宿主须在创建运行工作前调用并等待成功 |
| `status()` | 读取注册、扫描结果、延期原因和重验义务 |
| `runtime_status()` / model `memory_runtime/status` | 只读当前注意力与动作配置 |
| `semantic()` / `semantic_status()` | 可选语义求值器及独立状态 |
| `configuration()` / `reconfigure_evaluator(version)` | 显式 CAS 迁移，不自动重新布防旧 Watch |
| `actions()` | 获取可选动作运行时 |
| `set_enabled(bool)` | 显式开关，隔离宿主不能启用自动运行 |
| `schedule_recheck(Recheck)` | 持久化 defer、Skill review 或依赖重验时刻 |
| `observe_basis(&ProjectionBasis)` | 为已注册 Space 保留原生失效时刻 |
| `acknowledge_recheck(key, due_at_ms)` | 消费者完成处理后按键和原时刻确认 |
| `register_resume_verifier(condition, pin, verifier)` | 显式安装有时限的条件核验代码，重启后需重新安装 |

模型的布防工具走同一入口；已有原生注意力状态会在 Space 首次加载时登记。
从未登记的 Space 需要先加载/登记一次，不能凭已有内存 map 宣称其可恢复调度。
Space 的 learning 变更也会先登记；已启用 registration 的 review 时刻进入重验列表。
学习运行时在实际绑定和显式开关就绪后启动独立持有的学习推进任务，Watch 扫描无需等待
业务 I/O。原生直连宿主绕过注册时不属于此调度保证。

当前时刻的原生 BELIEF 结果可异步提示依赖失效时刻；原始 JSON、历史/假设查询和
模型自填时间不能冒充该来源。这只更新图外目录，不修改认知图/原生控制状态，也不
延长只读 KIP 工具等待。关闭时排空已接收的提示；周期 reconciliation 修复调度缓存。

## 动作回调与授权

在共享 AppState 或加载 Space 前调用 `with_action_bindings`。配置包含受信任的 Rust
对象；模型不能安装执行器、选择接收账号、提供凭据或修改权限。运行时的 policy
digest 同时固定策略、绑定、渠道、观察者和预算；配置变化不会自动接管旧布防工作。

| 接口 | 宿主职责 |
| --- | --- |
| `ActionIdentity::authenticate(scope)` | 每次返回当前认证身份，不保存凭据作为永久授权 |
| `ActionPolicy::context(wake)` | 由可信代码指定任务的必要上下文和前提 |
| `ActionPolicy::suggest(input)` | 从受限记忆包提出建议；只有此方法可由模型实现 |
| `ActionPolicy::authorize(request)` | 每次按当前业务规则审核固定请求 |
| `ActionPolicy::allow_silence(input, reason)` | 显式允许不打扰或已解决的判定，默认拒绝 |
| `ActionExecutor` | 固定目标和凭据，检查环境、工具、预算、真实权限及发送时租约 |
| `ActionLookup` | 使用同一 attempt 查询实际目标，并返回新鲜认证的观测 |

宿主须在布防前登记真实 Nexus Principal 和相应权限，可通过 `space.attention().actions().unwrap().nexus()`
访问原生治理接口；该 Rust builder 不自动授予权限；启动配置可单独显式启用上述 `bootstrap`。工作流使用 `read`、`read_history`、`read_governance_history`、`project`、
`create`、`update`、`derive`、`maintain` 权限，继续受资源约束限制。lookup 使用单独登记、直接认证且有
`record_outcome` 的主体。semantic actor、普通 Brain write token 和模型自述均不
构成这些权限；目标系统还必须独立检查自己的授权。

## 读取与决策

`anchor` 是已存在的 Proposition，仅用于取得真实 BELIEF 读取坐标，不假定其为真。
`premises` 独立声明必须当前 accepted 的前提。原生反证、冲突、有效时间和不确定性
保留在包中；必要引用、确切 SkillRevision、Watch/fire 和当前可见 Commitment 在
固定快照读取。任务特有、未表示为 Commitment 的约束由宿主列入 `required_refs`。

现有预算器整体保留必要项。约束页不完整、必需项缺失/被遮蔽、依赖失效、输入放不下
或前提未核实均不能产生执行许可；包仍标明语义不完整、不是执行授权。建议只能引用
实际交付的 used_refs；检索、使用与应用修订分别记录。应用修订须实际使用、匹配任务
族，并在执行前通过原生 current_revision、可执行权限和依赖校验。动作运行时不授予 Skill
standing，不执行或伪造学习试验。

| 决策 | 持久化结果 |
| --- | --- |
| act | 决策、普通尝试、固定请求工件和分派后继工作 |
| ask | ask 决策、固定问题/接收者/期限、发送和待答工作 |
| defer | 带原因决策、定时重试或人工恢复工作 |
| silence | 有宿主政策或已核实去重依据的决策，不发送 |

每个 gate 的 Decision/Attempt、后继 wake 与自身终结原子提交；提交前先确认不可变
请求工件及 CAS journal 中的完整准备记录，中断可能留下未使用工件。回执丢失通过
原生精确重放恢复，不创建第二份逻辑决策或 Attempt。
业务去重键绑定 Space instance、请求和配置；原工作未确认时保留 defer，不重复执行。

## 澄清

父决策保持 ask，不能绑定 Attempt。独立的确定性发送工作读取已提交的父决策，再创建
子 act 和 Attempt，把父决策作为真实、有版本的输入；不会调用建议模型或递归 ask。
发送使用单独的 `DeliverClarification` 渠道，不授予原业务操作权限。

`ActionRuntime::respond(gate_wake_ref, fresh_auth, ClarificationResponse)` 仅接受截止前、来自固定接收者的新鲜认证回答。同文可重试，异文冲突。
回答只作为数据进入新的业务 gate，必须重新验证当前政策；超时生成 defer 和人工事项，
不会视为同意，已过期问题也不会继续发送。发送 ACK 不完成独立后果义务。

## 分派与恢复

外发前校验不可变原生请求工件、当前政策/目标权限、最新 BELIEF/约束和确切上下文，
最后在有效 owner/fence 下通过 `begin_wake_dispatch`。外部 key 永远是已提交的 Attempt
身份。调用方取消不截断已拥有的提交；回调超时保留未知投递，不伪造成功或失败后果。

非幂等发送不确定时查询原目标；没有查询能力则明确 blocked。只有新鲜认证的权威
NotStarted 经原生对账，或经过验证的目标幂等契约，才允许同 key 重发。观察者及配置
在首次发送前固定。Unknown 不授权重发，Accepted/Finished 只是投递状态。分派 wake
等待独立终结 Outcome 对账，通用认证接收层由运行时 API 提供；不宣称外部副作用 exactly-once。

| 控制 | 含义 |
| --- | --- |
| `AttentionRuntime::runtime_status()` / model `memory_runtime {operation:"status"}` | 只读配置和扫描结果，就绪性须逐项验证 |
| `AttentionRuntime::actions()` | 获取本 Space 可选动作运行时 |
| `ActionRuntime::status(wake_ref)` | 持久状态与关联引用 |
| `ActionRuntime::set_enabled(false)` | 持久停止新动作处理，保留 Watch 发现与队列 |
| `ActionRuntime::retry(wake_ref)` | 人工重新开放有限重试窗口，不更换 attempt，不绕过准入 |
| `AttentionRuntime::set_enabled(false)` | 暂停整个 Space 的注意力调度 |

无绑定时保留 pending，并报告缺少配置。策略、绑定或 Watch 代次变化需明确的迁移/
重新布防审查，旧的未决/未知外发必须先对账。工件使用保留目录 `actions/` 和条件写入，
仍采用每 shard 单活动宿主；隔离 fork 不继承执行回调，关闭先排空动作写入再关库。

## 动作预算

安装后的默认上限：每 Space 每轮 2 项；记忆包 4,096 token，完整 gate 输入 32,768，
建议 2,048；回调和完整读取阶段各 5 秒；真实租约 60 秒，初始重试间隔 60 秒。自动 gate
重试最多 3 次，发送/查询共用有限窗口，耗尽转人工。模型适配器还需约束其真实模型
请求/输出。原生/存储提交必须完成，不能以超时截断。未安装回调时不发生模型调用或外发。

回归使用真实 Nexus/AndaDB 事务和本地确定性适配器，覆盖四分支、上下文不足、当前
权限/修订、澄清、CAS/回执丢失、原生 PUT 暂停、冷恢复、lookup 及公平调度。测试不
调用网络模型，不将机制通过解释为模型效果、学习收益或生产适配器已验收。

## 存储与恢复

保留前缀 `__brain_runtime__/v1/shards/{shard}/` 存放 CAS 根、固定序号索引、带版本的注册、分页检查点及退避记录。空槽允许
表示布防前中断的分配；只有注册和索引都持久化并读回后，才允许创建原生工作。
外部 Space ID 与 Nexus 保留的 attention scope 明确映射；实际 KIP 操作使用
`DEFAULT_SPACE`，二者名称不混用。instance、shard、格式或身份不符时拒绝运行。

目录与 Nexus 不假装具有跨库事务：先登记，再原生提交；完整扫描只能确认相同版本
和 dirty 代次。分页中断保留 Watch 位置及未处理 wake ID；较旧扫描不能覆盖新工作。
所有注册槽都会轮转核对，due 缓存不能永久隐藏工作。结构化与诊断扫描分开，文本、
legacy 和失败恢复保留原因及退避，不自动重新布防。

需要支持条件写入的存储；CLI 本地模式已包装 MetaStore，裸 LocalFileSystem 不具备
所需 CAS。每个存储 shard 只能有一个活动 Brain 所有者进程，未提供跨主机多写者接管。

## 预算与生命周期

| 默认值 | 值 |
| --- | --- |
| 全局周期 | 5 秒 |
| 每轮槽数 | 20 |
| 每 Space 结构化页面 | 20 |
| 诊断页面 | 20 |
| 单 Watch 变更页 | 200 |
| wake 物理版本页 | 200 |
| 本轮启动新操作的时间预算 | 10 秒 |
| 周期核对与退避 | 60 秒 |
| 未安装动作绑定的模型与外发预算 | 0 |
| 每运行时并发队列上限 | 16 |
| 每 Space 重验义务上限 | 128 |
| shard 索引槽上限 | 1,000,000 |

时间预算控制是否继续启动操作，不截断已开始的原子写入；条件回调在原生写锁外受
超时限制。停机先停止接收新操作，再排空任务、恢复和关闭数据库。`is_busy()` 计入
已观察到的活动租约；Formation/Maintenance 继续使用各自门控。注意力冷加载不启动
Formation/wiki 恢复、不刷新用户访问时间；后续正常访问仍能恢复这些队列。显式安装的
动作回调有独立预算，可在冷加载后运行。

机制测试不依赖在线模型；20 Space/200 Watch、5 秒 tick 的基准验证 P95 ≤ 60 秒
门槛。部署的网络/存储延迟需另测；服务停机后恢复补跑，不承诺停机期间准点执行。

## 后果恢复

图外 CAS journal 在变更前保存原始认证输入与原生 intent，调用方取消不截断已接收
写入。丢失 ACK 用新鲜认证重放原事件。原生留证和分派对账是两个可恢复步骤，不制造
第二份 Outcome。审计索引先确认存储再发布不可变快照，消费者按收据 ID/native ref
去重；`ConsequenceRuntime::receipts(auth, lane, after, limit)`有界发现当前允许读取的审计和安全事项。

输入上限 16 KiB、事件键 256 字节、运行时写入准入 16 项、每个观测索引最多一百万
事件。测量载荷、迟到/更正处理及学习路由见[中文 API](API_cn.md#有身份的运行时待办与后果)。

## 发布范围与验证

KIP v2 尚未上线，本次以新建 v2 Space 为发布基线，不安排旧版本库存盘点或开发期
孤儿 Watch 迁移。依赖后台调度前须注册 Space；正常重启、驱逐、确认丢失恢复与配置
变更仍属于当前支持范围。

状态报告提供有界扫描/工作计数、原因和持久引用，不是完整运行库存，也未提供跨阶段
延迟、积压年龄与成本的汇总指标。生产存储/模型时延须单独测量；机制测试验证恢复和
授权，真实学习改善、语义漏判率、utility 归因和 trust 校准须在 MIB 用独立业务数据
验证后，再启用相应自动行为。

实施顺序、指标定义和验收门槛见[运行观测与效果验证方案](../VALIDATION_PLAN_cn.md)。
