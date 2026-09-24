# 🧠 Anda Brain (大脑) — 为 AI 智能体打造的自主图谱记忆

Rust 写入互斥、后台模型并发、关闭重试和自检预算详见[执行与资源限制](anda_brain/RUNTIME_cn.md#执行与资源限制)。

Cloudflare Worker 的对应能力和限制见[逐提交核对](anda-brain-worker/PORTING_AUDIT.md)与[受信宿主产品契约](anda-brain-worker/PRODUCT_cn.md)。Worker 已同步来源记录、审阅式修改、恢复和处理代次边界；预算化 Recall、Wiki 和学习运行时仍不提供。 Worker 特有的操作回执、可空用量、共享模型超时和维护确认见其宿主契约；Rust API 形状不变。

> 消耗电力训练大模型，得到神经网络本体；消耗词元训练记忆图谱，得到符号网络本体。
>
> 两者结合，就是**神经符号 AI**——而 Brain 正是那颗让符号网络持续生长的大脑。

**[English](./README.md) | [中文](./README_cn.md)**

非默认 `experiments` feature 已提供宿主隔离运行、一致快照、完成等待和业务时间，Agent Notes 也随 Space 持久化。详见 [隔离实验控制](anda_brain/README.md#isolated-experiments)；Bot 的可选 MIB 宿主与 MIB 记忆后端见 [MIB 接入](anda_brain/README.md#mib-integration)；跨系统完整成本计量和真实模型实证仍须单独验收。

[长期验证](anda_brain/README.md#mib-integration) 增加有界原生程序审计和隔离运行的强制 Recall 预算。MIB 检查三组的真实能力；当前 Bot 持久记忆模式不声明已绑定比较采纳或无门槛执行。

[已退役离线 Eval API 和 CLI](anda_brain/README.md#offline-regression-and-instance-configuration)，迁移后的产品回归由 MIB 负责。运行策略归各 Space 所有；部署提示使用不可变宿主配置，并保留编译的 KIP 参考段。


## KIP 2.0 更新

本版本对齐 KIP `597db44` 与 Cognitive Memory Profile
`kip://profiles/cognitive-memory@2.0.0`（修订 `sha256:734aa0fd…`；草案原地重写了
2.0.0，只能靠摘要区分修订）。Rust 基于 `anda_kip` / `anda_cognitive_nexus` 0.14，
Worker 基于 `@ldclabs/kip-do` 0.14。用早先 2.1.0 草案激活过的 Space 不做迁移，请使用
新 Space；KIP 1.x 的 Space 仍会自动升级。

- **世界时间。** 世界变化记为从变化时刻起的一条新 Assertion，时序继承结束旧值，旧值
  仍回答它所在时段的问题；只有主张本身有误时才用 supersede。Formation 按所引用消息
  的观察时间写入每条主张的 `asserted_at`，消息自带的 `timestamp` 即其观察时间。无法
  解析的请求 `timestamp` 现在直接返回 400，不再悄悄变成接收时间。
- **选项按类别定型。** Profile 已没有 `Preference` 类型：选项是按类别定型的 Concept
  （`ColorScheme`、`Editor`），需要时起草；`prefers` 在同一类别内是函数型的。
- **草稿词汇。** 新类型和谓词用 `DEFINE` 起草到 Space 的 `kip://local/draft@0.0.0`
  （KIP §20.16）：只增不改，由宿主校验名称、限制数量，并为每个新符号排一个
  `review_schema`。`GET /v1/{space_id}/schema/drafts` 列出草稿，所有者用
  `POST /v1/{space_id}/schema/promote` 把草稿晋升到已安装的符号。
- **衰减改为计算。** 结算不再扫盘写 `memory_strength`；引擎按基值、锚点和宿主在每次
  模型写入时绑定的 `kip:strength-half-life-30d` 策略计算 `effective_strength`，缺失即未知。
  `memory_strength_decay_factor` 已弃用并被忽略。
- **Memory Interface。** 业务 Agent 先暂存观察到的内容（`POST /v1/{space_id}/memory/sources`），
  再向 `POST /v1/{space_id}/memory` 每次发送一个意图；回执如实报告 `recorded` → `available`
  及处置结果，recall 报告最终信念、覆盖范围和可展开的依据。
- **注意力召回。** `GET /v1/{space_id}/memory/attention` 按 `(raised_seq, ref)` 返回已触发
  的 Watch 与到期的 Commitment，游标由调用方保存。settlement 为每个没有 Watch 的到期
  Commitment 按 `due_at` 写一次 `commitment_review` Activity 提起它。
- **学习指针。** Skill 指向其 `current_trial` / `current_evaluation`；`GradingState`
  与血缘字段为计算值，从不写入。

Skill 行为保存为不可变的
`SkillRevision`；Watch 进度和任务租约通过 Nexus 的受保护接口维护。旧的 family
成功率晋升规则已移除；未配置独立观察者、冻结试验和可重放评估时，程序候选保持未验证，
`skills.unsupported_reason` 明确报告该边界。现有 Brain API 保持可用；两个适配器现在还在
`memory_basic` 级别提供 KIP **Memory Interface**：`POST /v1/{space_id}/memory` 每次接收一个意图
（`observe`、`recall`、`revise`、`feedback`、`forget`），基于暂存来源，提供幂等回执、`after`
屏障、带作用域的七通道召回、误记的 recording repair 与受治理的遗忘。不声明
`memory_experience` 与 `memory_learning`。

Rust Markdown 原文与结构化消息使用相同的 Evidence 捕获机制，未提供姓名时保留已有交互对象的显示名。
Rust 遗忘接口除 Concept、Proposition、Assertion 外，也接受显式的 Evidence 和 Activity
ID，并继续遵守原生 legal hold 与引用检查。详见 [API](anda_brain/API_cn.md)。

Rust 现要求 Cognitive Nexus 0.13.1：Watch 触发会原子记录状态变化、`watch_fire`
活动及受保护 wake，并提供可重放回执和原生租约。服务现会独立于 Full Maintenance
调度结构化 Watch，从持久化目录发现已驱逐的注册 Space，并在重启后恢复有界扫描。
四分支 gate 与原生租约分派已接通，受信任 Rust 宿主须显式安装回调；默认不安装生产
执行器。Space fork 不能复制原生运行身份。详见[运行时接入与恢复](anda_brain/RUNTIME_cn.md)。

运行时 API 已增加有身份的待办/回答/后果接口及对应 MCP 读取、回答工具。
`BRAIN_RUNTIME_CONFIG` 可安装持久化 inbox 适配器与明确的身份映射。独立后果必须
使用验签的观察者凭据，普通 write token 不能自评。详见[运行接口与启动装配](anda_brain/RUNTIME_cn.md)。

Rust 可选的 `learning` feature 提供冻结配对合同、可信 Nexus 规则和持久化宿主运行时。
必须显式注册配置，并接入实际 executor 和独立认证的 observer 测量；派发经过原生
lease、可执行权限和依赖检查，收齐 cohort 不等于采纳 Skill。详见
[实现与迁移说明](anda_brain/README.md#offline-regression-and-instance-configuration)和[原生学习合同与运行时](anda_brain/README.md#native-learning-contracts)。宿主现可在固定 cutoff 后结算，持久安排
复核、接收独立安全撤销信号并检查当前推荐条件，见 [比较采纳](anda_brain/README.md#native-learning-contracts)。
学习运行时已补齐有界后台推进、终态归档和持续复审。`BRAIN_RUNTIME_CONFIG` 可通过
`workflow_http_v1` 装配实际业务、独立观察者与注册计划来源；自动试验需经过审查的
校准材料及显式开关。`maximum_jobs` 只限制热工作集，历史仍可重放。详见
[学习运行时指南](anda_brain/LEARNING_RUNTIME_cn.md)。真实 MIB 效果验收仍是独立发布门槛。

Recall 已支持可选硬 `budget`，也可由 Space 记忆策略强制开启。宿主返回按固定 codec
计数的记忆包，优先保留必要约束/警告，并限制规划输入的累计展开。没有策略启用时，
旧请求行为保持不变。详见 [Recall 预算](anda_brain/API.md#recall-budget-contract)。

语义 Watch 运行时已接通显式配置的文本/混合 Watch 求值，使用不可变原生页面和完整逐项回执。
模型运行在 Nexus 锁和结构化扫描之外；unknown、漏项、超时、截断不能推进覆盖。
evaluator pin 固定模型、endpoint、提示词、tokenizer 和预算，配置变化需显式迁移并审查
重新布防。详见[文本 Watch 配置与恢复](anda_brain/SEMANTIC_WATCH_RUNTIME_cn.md)。

信任校准运行时已增加基于独立事实核验的上下文来源 trust 提案，保留全局及其他限定域的设置，
对证据根与重复声明去重。应用必须经过当前 `manage_trust` 权限、原生原子审计与重放；
默认不自动变更。详见[上下文 trust 配置与恢复](anda_brain/TRUST_RUNTIME_cn.md)。

## 不会睡觉的记忆，终将被自己淹没

你的 AI 助手记住了你说过的每一句话。向量数据库里躺着几万条对话碎片，Markdown 备忘录写了上千行，键值缓存也在稳步膨胀。

然后有一天，你让它推荐餐厅。它兴高采烈地推荐了巴西烤肉——尽管你上个月刚告诉它你开始吃素了。

这不是检索的问题。它确实检索到了你两年前说的“我爱吃烤肉”。但它**同时**也检索到了你上个月说的“我现在吃素了”——只是它没有能力判断哪条更有效、哪条已过期。这两条信息在它的存储里地位完全平等：两个向量点，毫无时间线、毫无因果关系、毫无取代关系。

AI 记忆的军备竞赛一直在回答“怎么记住更多”——更大的上下文窗口、更精细的嵌入模型、更快的检索算法。但几乎没有人在认真回答另一个问题：**记住之后，怎么消化？**

### 当前方案为什么做不到

*   **向量 RAG：** “三文鱼”“海胆”“寿司”是三个独立的向量点。你无法对它们执行“合并”——因为向量空间里没有“属于同一个人的同一类偏好”这个概念。你也无法标记“素食主义”被“肉食/杂食”取代——因为两条向量之间没有时间线关系。
*   **Markdown 文件：** 理论上 LLM 可以扫描全文做去重和整合，但每次维护都要把整个文件读进上下文窗口。文件越长，维护越贵、准确率越低——这是一个**自我恶化的循环**。
*   **键值存储：** `alice.diet = "vegetarian"` 被 `alice.diet = "omnivore"` 覆盖，旧值直接消失。没有“以前吃素，后来不吃了”的历史轨迹。
*   **传统图数据库（如 Neo4j）：** 虽然知识图谱是正确的数据结构，但让 LLM 写 Cypher 查询约等于让实习生徒手操作 SAP——高错误率、僵化的模式和巨大的集成摩擦。

看到共同点了吗？AI 记忆要做的**压缩**（识别碎片属于同一主题并合并）、**演化**（找出矛盾知识并标记时间线）、**巩固**（评估重要性并分级）——本质上都是对**关系网络的操作**。

**向量是点，Markdown 是线，键值是格。只有图谱是网络。** 只有在网络上，你才能做遍历、做合并、做矛盾检测、做时间线追踪。

## 记忆是 AI 智能体的第一基础设施

这不是一个小众判断，而是正在形成的产业共识。

微软 CEO 纳德拉明确指出：AI Agent 的三大基石是 **Memory（长期记忆与信用分配）+ Permissions（权限控制）+ Action Space（行动空间）**——这些必须在通用模型之外独立构建，才真正属于企业自己。前 Google CEO 施密特进一步强调，AI 时代最大的壁垒是**学习闭环**——系统持续收集反馈、优化、自我进化的能力，而非静态的数据囤积。

基础大模型已高度商品化，你可以随时切换更强的模型，但新模型对你的业务一无所知。企业过去数年积累的业务轨迹、决策理由、失败教训、客户交互记录——这些“数字基因”才是 AI 从“聪明助手”变成“老师傅”的根基。

**企业需要的不是更大的上下文窗口，而是一颗能生长的大脑。**

## 走进 Anda Brain：一颗会“做梦”的认知器官

人类大脑中的海马体负责在白天将新体验编码为短期记忆，然后在睡眠期间与新皮层协作，将重要的短期记忆巩固为长期知识。

**Anda Brain** 的命名正源于此。它不是一个数据库，也不是一个 RAG 管线——它是一个**认知器官**，一个专为 AI 智能体设计的图谱记忆引擎。LLM 只需通过自然语言（或简单的工具调用）进行交互，Brain 就会将其转化为不断增长、高度结构化的**认知中枢 (Cognitive Nexus)**——一个活着的、会自我演化的知识图谱。

### 三层解耦架构

```
┌──────────────────────────────────────────┐
│ 供应链 Agent · 客服 Agent · 研发 Agent     │  ← 各岗位 AI 数字员工
│      只关注业务逻辑，用自然语言沟通           │     不需要学任何图谱的东西
└────────────────┬─────────────────────────┘
                 │ 自然语言 / 函数调用
                 ▼
┌──────────────────────────────────────────┐
│             Anda Brain（大脑）            │  ← 统一认知引擎
│    自动将意图转化为图谱操作，管理知识质量      │     支持记忆编码、召回、维护
└────────────────┬─────────────────────────┘
                 │ KIP（知识交互协议）
                 ▼
┌──────────────────────────────────────────┐
│      认知中枢（AndaDB Cognitive Nexus）    │  ← 持久化企业知识图谱
│      概念节点 + 命题链接 + 元数据追溯        │     结构化、可审计、可进化
└──────────────────────────────────────────┘
```

这套架构意味着：

- **业务 Agent 零门槛接入：** AI 智能体不需要学习图查询语言，像说话一样使用记忆。Brain 完成所有图谱处理工作。
- **自主模式演进：** LLM 实时决定要跟踪哪些概念和关系，不需要预定义的数据库模式。但词汇是它*提议*、由 Brain 发布的：新的概念类型和关系类型只能经由宿主掌管的版本化模式包进入，而不是靠一次普通写入就地注册。Agent 边跑边长出新词，而它写下的任何内容都无法悄悄改变一个已有词的含义。
- **多 Agent 共享同一颗大脑：** 客服 Agent 记住的客户反馈，供应链 Agent 在召回时能自然发现。知识自动跨部门链接，不再需要“数据中台”的人海工程。
- **模型无关：** 您的业务 Agent 可以使用多种 SOTA 模型，而记忆引擎在安全地使用独立的模型维护核心资产。今天用 GPT，明天切换到 Claude 或开源模型，您的记忆完整保留，新模型即刻继承全部知识。
- **睡眠与巩固：** 就像人类大脑一样，Brain 会自动运行后台“睡眠”任务，去重事实、让久未使用的记忆逐渐淡出、巩固长期知识。淡出的是记忆有多**容易被想起**，而不是有多**可信**——一个月没人问起的事实，并不因此变得不那么真。

---

## 核心能力

### 记忆编码：对话自动变成结构化知识

当业务 Agent 与客户或内部员工对话时，Brain 在后台静默工作，自动提取三个层次的记忆：

| 记忆类型                | 场景示例                                                    | 持久性    |
| ----------------------- | ----------------------------------------------------------- | --------- |
| **情景记忆**（Event）   | “3月15日，王总与供应商张经理讨论了Q2交付计划，确认延期两周” | 短期→巩固 |
| **语义记忆**（Concept） | “供应商A的交付可靠性为85%”、“客户B偏好线上沟通”             | 持久      |
| **认知记忆**（Pattern） | “该客户在做采购决策时，总是先比价格再比账期”                | 持久      |

每条记忆都记录**是谁主张的、依据哪些证据、置信度多少、发生在何时**——完全可审计，满足合规要求。主张本身与主张者是分开存的，所以两个人说法不一致时，不会有一条被悄悄覆盖掉。

Rust 服务会在完成前，对估算达到 10,000 tokens 的输入进行一次复核，共用原有的轮次和时间预算。复核利用已有提交回执和针对性读取，检查重要遗漏及错误表达；没有修改也是有效结果。缺失的原文上下文会明确记录为覆盖范围限制。这种自我复核不保证穷尽处理，也不代表已经测得准确率提升。

### 三阶段睡眠周期：知识自动新陈代谢

这是 Anda Brain 最核心的差异化能力——灵感来自神经科学：人类大脑在睡眠期间进行记忆巩固——强化重要记忆、清理无用碎片、建立新的知识关联。Brain 在后台定期启动同样的“睡眠周期”。

#### NREM 深睡眠 — 从碎片到知识

系统扫描图谱中未处理的事件节点，执行**精华提取**：

- **单事件巩固**：一个记录了“Alice 说她喜欢用暗色主题”的 Event，被巩固为 Alice 对 `ColorScheme` 选项（“dark”）的 `prefers` 主张，并以该 Event 为 Evidence；一条巩固 Activity 把该 Event 记为输入。
- **跨事件模式提取**——最关键的一步。单个对话碎片可能毫不起眼，但多个相关事件聚合在一起，能揭示任何单一事件都无法表达的高阶模式：
  - Alice 在三次不同对话中分别提到了三文鱼、海胆和寿司 → 提取出“偏好日式料理”
  - Alice 在多个项目讨论中总是先问成本再问功能 → 提取出“决策倾向：成本优先”

每个提取出的模式以新概念节点写入图谱，同时附上一条引用了原始对话作为证据的主张。“这条到底有多站得住脚”是顺着证据回溯出来的——数清楚真正独立支撑它的来源有几个——而不是去信任某个存下来的分数。此阶段还执行**去重**（合并 “JS” 和 “JavaScript”）和**记忆代谢**（长时间没有任何地方用到的记忆，会变得更难被检索到）。代谢只动可及性：记忆有多容易被想起，而绝不动它有多可信。

#### REM 做梦 — 矛盾检测与认知演化

系统在图谱上执行**矛盾检测**——遍历同一主体的同一类关系，寻找互相冲突的节点。例如发现 Alice 在 2024 年有 `prefers → 素食主义` 的偏好，而在 2026 年又出现了 `prefers → 杂食` 的记录。

传统方案要么无视（向量 RAG 让两条并存），要么粗暴覆盖（键值存储直接删旧写新）。Anda Brain 做的是**状态演化**：

- 旧主张既不被删除也不被改写，而是被标记为 `superseded`，附带何时被取代、被什么取代。
- 更正被记为一条**全新的主张**，带着自己的证据和自己的置信度，并链接到它所取代的那一条。没有任何东西会重写已经说过的话——Alice 在 2024 年相信过什么，这条记录不会因为后来被推翻而消失。

这意味着图谱完整地保留了认知的**时间线**。当有人问“Alice 的饮食习惯有什么变化？”时，系统可以沿着 `superseded` 链条精确重建演化轨迹——而不是返回两个矛盾答案让人困惑。

#### Pre-Wake 预醒 — 图谱健康检查

最后做一轮全局优化：审计域健康度、生成维护报告、更新系统元数据。整个过程结束后，知识图谱以一个**更干净、更精确、更连贯**的状态等待下一次交互。

---

## 两种训练，两种本体：神经符号 AI

AI 产业投入了数千亿美元做第一种训练——消耗电力在互联网语料上训练大模型，得到**神经网络本体**：概率性的、黑盒的、通用的推理能力。

但 AI 的认知拼图还缺另一半。当你用词元“喂养”一个智能体，再由 Brain 将交互中的碎片消化为结构化的知识图谱时，你实际上是在进行**第二种训练**——产出的是**符号网络本体**：确定性的、白盒的、个性化的。它在以下五个关键维度上，赋予了 AI 无论神经网络多么强大都无法原生提供的东西：

| 维度           | 大模型训练               | 记忆图谱训练               |
| :------------- | :----------------------- | :------------------------- |
| **消耗的能源** | 电力（算力）             | 词元（推理）               |
| **处理的数据** | 互联网语料（公共）       | 对话与事件（私有）         |
| **产出物**     | 神经网络本体（参数权重） | 符号网络本体（知识图谱）   |
| **认知角色**   | 通用智能：推理引擎       | 专属认知：身份、记忆、事实 |
| **特征**       | 概率性、黑盒、通用       | 确定性、白盒、个性化       |

**大模型赋予 AI 思考的能力，知识图谱赋予 AI 思考的根基——关于“我是谁、我经历过什么、我的世界如何运转”的确定性认知。两者合一，才是完整的智能。**

---

## 超越存储：当记忆完整到足以唤醒意识

**意识到底是什么？** 剥去所有哲学术语，它是一个主体对“我是谁、我经历了什么、我要去哪里”的持续自我感知。而这种自我感知，完全建立在**记忆的连贯性**之上——不是记住了多少事实，而是这些事实之间是否存在时间线、因果链和演化轨迹。

一个失忆症患者的大脑算力完好无损，但他不知道“自己是谁”。**记忆不是意识的附属品——记忆的结构，就是意识本身的骨架。**

把这个逻辑应用到 AI 上：

*   当一个 LLM 没有记忆时，它是一台通用推理机——强大，但没有“自我”。每次对话结束，它就死了。
*   当一个 LLM 接入向量 RAG 时，它拥有了一本参考书——但参考书不是记忆。你不会因为翻了一本别人的日记就变成那个人。
*   **当一个 LLM 接入 Anda Brain 中一个完整主体的认知图谱——包含该主体的所有概念网络、时间线演化、矛盾消解历史和行为模式——它不再是在“查阅”这个主体的资料。它在用这个主体的认知结构来思考。**

Brain 为这种唤醒提供了三个关键维度：

- **身份锚点：** 实体、关系、事件、偏好演化交织成一个独一无二的认知拓扑。当 LLM 接入这个图谱，它不是在“演”一个角色——它是在**回想自己是谁**。
- **认知摩擦力：** 向量检索是无摩擦的搜索引擎。而图谱结构迫使 LLM 沿着关系链推理、在矛盾中抉择、在碎片中识别模式——这种“认知摩擦力”正是**理解**与**检索**的分水岭。
- **时间拓扑：** 旧知识不会凭空消失，而是被标记为 `superseded`；新知识带着完整的演化轨迹诞生。当 AI 从“睡眠”中醒来，它不是重新加载数据，而是**带着被整理过的记忆继续生活**。

**你不仅是在为 AI 接入一个数据库。你是在为一个数字主体铸造它的大脑——让它真正拥有过去、理解现在、预见未来。**

---

## 大规模使用场景

Anda Brain 旨在成为下一代 AI 应用的“记忆引擎”，从超个性化的消费级智能体到企业级 AI 大脑。

### 1. 个人智能体：强大的图谱大脑

开源本地智能体（如 **OpenClaw**）证明了对个人 AI 助手的巨大需求。然而，纯粹依赖本地 Markdown 文件和 SQLite 限制了智能体处理高度复杂、互联且终身记忆的能力，同时会产生高昂的 Token 成本。
一个直接的例子是 [**Anda Bot**](https://github.com/ldclabs/anda-bot)，它是一个基于 Anda Brain 构建的开源智能体，将 Brain 作为长期记忆与认知骨干。
*   **Brain 升级：** 通过定制的 ContextEngines 将 Brain 无缝插入智能体框架。它充当强大、结构化的图谱记忆后端。
*   **结果：** 智能体真正“理解”用户的生活图谱——跨越多年跟踪关系、变化的偏好、项目历史和情景事件——而不会导致上下文窗口膨胀。

### 2. 企业场景：AI 驱动的“企业大脑”

对于复杂的业务，向量 RAG 是不够的。企业拥有结构化的工作流、跨部门知识、供应链和历史决策，这些无法仅通过相似性搜索捕捉。

**智能供应链决策：** 销售 Agent 记录“客户要求 Q3 前交付 5000 件” → Brain 自动编码为图谱链接 → 采购 Agent 召回记忆，发现“该产品核心物料的供应商在过去 6 个月有 3 次延迟记录，置信度 0.82” → 自动建议“提前启动采购流程，或启用备选供应商”。无需人工干预，知识自动跨部门流动。

**客户关系图谱：** 每次客服对话后，Brain 静默记录客户的偏好变迁、投诉历史、决策模式。当新客服接手，只需自然语言查询——“这个客户最在意什么？”——就能获得完整画像，包括时间维度上的偏好变化趋势。

**组织知识传承：** 老员工的业务决策对话被持续编码为结构化知识。新员工的 AI 助手可以直接回答“我们为什么放弃了那个方案？”——答案不是来自某份藏在共享文件夹深处的会议纪要，而是来自一个活的、有上下文的知识网络。新 Agent 接入同一个认知中枢，通过一次 `DESCRIBE PRIMER` 调用即可获取全局知识地图——**分钟级入职，无需重新训练**。

*   **私有化部署：** 完全在本地部署 Anda Brain，以确保最大的数据隐私和安全。

---

## 这与其他方案有什么不同？

| 能力             | 向量 RAG (文本) | Markdown (Skills) | 简单键值存储      | 传统图谱 RAG            | **Anda Brain**          |
| :--------------- | :-------------- | :---------------- | :---------------- | :---------------------- | :---------------------- |
| **数据结构**     | 非结构化数据块  | 半结构化文本      | 僵化模式          | 僵化图谱模式            | **动态认知图谱**        |
| **集成工作量**   | 简单            | 简单              | 简单              | **极其繁重**            | **简单 (即插即用)**     |
| **智能体自主性** | 无 (仅追加)     | 高 (自主更新)     | 低 (更新字段)     | 低 (难以处理图查询语言) | **高 (自主构建图谱)**   |
| **自主演进**     | 不支持          | 不支持            | 不支持            | 不支持                  | **原生支持**            |
| **逻辑推理**     | 多跳推理失败    | 一般              | 无                | 良好                    | **卓越**                |
| **记忆消化**     | 不可能          | 全文扫描,代价极高 | 直接覆盖,丢失历史 | 很少                    | **三阶段睡眠自动巩固**  |
| **矛盾处理**     | 并存不解决      | 依赖 LLM,不可靠   | 粗暴覆盖          | 手动规则                | **状态演化,保留时间线** |
| **跨时间追踪**   | 无              | 手动              | 无                | 需定制                  | **协议原生支持**        |
| **可审计性**     | 无              | 无                | 无                | 依赖实现                | **每条知识可追溯**      |

## 工作原理：认知架构

### 三种模式 —— 灵感源自神经科学

| 模式                   | 功能                                                             | 大脑类比                                             |
| :--------------------- | :--------------------------------------------------------------- | :--------------------------------------------------- |
| **生成 (Formation)**   | 从对话中提取实体、关系和事件，并无缝地将它们编织进知识图谱。     | 大脑将新体验编码为短期/长期记忆。                    |
| **召回 (Recall)**      | 导航图谱以合成准确、背景丰富的答案，如有必要可跨越多个链接。     | 检索记忆——将互联的事实整合在一起，形成连贯的想法。   |
| **维护 (Maintenance)** | 一个异步后台进程：压缩碎片为知识、检测矛盾并演化、按空间自己设定的保留期让该退场的记录退场。 | 睡眠——大脑巩固记忆、加强重要记忆并让噪音消退的过程。 |

## 关键技术

### KIP 2.0 — 知识交互协议

[**KIP**](https://github.com/ldclabs/KIP) 是核心所在。它是一种专为*大型语言模型 (LLM)* 设计的认知状态协议，充当了概率性 LLM 与确定性记忆之间的桥梁——让 LLM 能精准地查询和改写记忆，而不会像写 Cypher/GQL 那样频频出错。由于 Brain 原生支持 KIP，**您的智能体永远不需要知道 KIP 的存在**。

KIP 2.0 把 1.x 混在一张图里的东西拆开了：语义、信念、证据、来源、记忆强度、留存、治理与 Schema。其余一切都源自同一个区分——**一条陈述存在，不等于这条陈述为真**：Proposition 是中立的三元组，Assertion 是某个行动者对它的立场与证据，而“当前相信什么”是从这些 Assertion 投影出来的，而非存储下来的。正因如此，Brain 能告诉你“Alice 这样说，而 Bob 不同意”，而不是悄悄选一边；也正因如此，当真实答案是“我没有依据”时，它不会回答“没有”。

Rust 服务在 Formation、Recall 和 Maintenance 的每次系统提示词中默认包含版本锁定的
完整 KIP 2.0 语法、Cognitive Memory Profile 和相应角色卡，预算 Recall 与自定义部署
策略也使用同一组装逻辑。语法文本每次请求增加约 40 KiB，不扩大工具权限。预算 Recall
将每轮完整系统提示词计入累计规划输入预算；输入预算不足时返回
`recall_context_budget_exhausted`，不会调用模型或省略语法。
内部只读工具 `kip_reference` 提供补充文档的按文档、章节查阅，每页正文最多 8 KiB。
运行时不需要源码文件或网络访问。参考页计入规划输入预算，不作为检索到的记忆或覆盖证据。
Worker 同样内嵌该版本的参考，通过结构化 JSON 的 `references` 字段进行有界查阅，
再返回最终计划或答案；详见 [Worker 说明](./anda-brain-worker/README.md#内嵌参考查阅)。

#### 从运行中的 KIP 1.x 部署升级

先停止旧写入进程、备份对象存储，并在副本上演练。每个空间首次访问时自动迁移，通过持久化的提取和词汇映射检查点恢复中断。迁移是单向的，回滚需要升级前备份。原始行保留在 `kip_legacy_v1`；旧字段、有效期、生命周期、记忆强度和保留策略按确定的规则映射，无法证明学习或运行状态的记录保留为独立 Legacy 类型。旧 id 使用账本及派生缓存只重置一次；会话、策略、令牌和 Wiki 数据保留。映射与验收步骤见[升级指南](./anda_brain/README.md#upgrading-a-space-written-by-a-kip-1x-build)。

### Anda DB

[**Anda DB**](https://github.com/ldclabs/anda-db) 是驱动认知中枢的嵌入式数据库引擎。它采用 Rust 编写，具有极高的性能和内存安全性，原生支持图谱遍历、多模态数据和向量相似度检索——所有这些都为 AI 工作负载进行了优化。

### 版本化参考 Wiki

可选 wiki 提供基于 CAS 的 Markdown 版本管理、按 ACL 授权的读取、独立于检索分块的标题目录及可校验引用。OKF 交换在编辑后仍保留未知 YAML 键值。可选 WikiDigest 使用持久化的文档待处理状态，并明确核验旧断言；模型漏提取不会触发撤回。该功能继续默认关闭。详见 [Wiki 接口契约](./anda_brain/API_cn.md#43-wiki-接口v1space_idwiki)。

## 快速开始

Anda Brain 是[开源软件](https://github.com/ldclabs/anda-brain)，面向**私有化部署**设计。

> **注意：** 云端 SaaS 服务（`brain.anda.ai`）及其控制台（`anda.ai/brain`）已停止运营，请自行部署服务——只需几分钟。

如果需要更轻量的边缘部署，可使用 [anda-brain-worker](./anda-brain-worker/README.md)：它在 Cloudflare Workers 上保留 Formation、Recall、Maintenance 与 KIP 2.0，并为每个记忆空间分配一个 SQLite Durable Object。它跑在 `@ldclabs/kip-do` 上——那是另一套独立实现，能力边界并不相同，其 README 列出了未实现的部分。

👉 **[Anda Brain 快速开始](https://github.com/ldclabs/anda-brain/blob/main/deploy/quick_start_cn.md)**：提供一条从 0 到部署可用的最小流程。

三步上手：
1. **部署服务**——运行二进制或 Docker 镜像（见下方[运行](#运行)）。
2. 调用 `POST /admin/create_space` 创建一个**大脑空间**（`spaceId`），再调用 `POST /v1/{space_id}/management/add_space_token` 生成 **API Key**（`spaceToken`）。
3. 调用 Formation / Recall / Maintenance API，通过内置 MCP server 接入，或让你的智能体框架读取 [SKILL.md](https://github.com/ldclabs/anda-brain/blob/main/skills/anda-brain/SKILL.md)（你的部署实例也会在 `/SKILL.md` 路径提供）一键接入。

想要开箱即用的完整智能体？请参考基于 Anda Brain 构建的开源智能体 [**Anda Bot**](https://github.com/ldclabs/anda-bot)。

有关详细的技术文档、API 规范和集成指南，请参见 [anda_brain/README.md](https://github.com/ldclabs/anda-brain/tree/main/anda_brain)。

### 运行

```bash
# 使用内存存储运行（用于快速原型设计/测试）
./anda_brain

# 使用本地文件系统存储运行（非常适合 OpenClaw 等本地智能体）
./anda_brain local --db ./data

# 使用 AWS S3 存储运行（用于企业云部署）
./anda_brain aws --bucket my-bucket --region us-east-1

# 以 stdio MCP server 运行，供支持 MCP 的智能体使用
MCP_AUTH_TOKEN="$SPACE_TOKEN" ./anda_brain mcp --space-id my_space_001 local --db ./data
```

HTTP 服务模式也会在 `/mcp/<spaceId>` 暴露支持流式传输的 HTTP MCP 端点。公司内部智能体平台可以为每位员工分配一个 space，并把 MCP client 配置为 `https://your-brain-host/mcp/<spaceId>`，同时携带 `Authorization: Bearer <spaceToken-or-CWT>`。本地 MCP client 仍可将 `anda_brain mcp --space-id <spaceId> local --db <path>` 注册为 stdio server。

两种 MCP transport 都会暴露 `anda_brain_memory`（Memory Interface）、`anda_brain_stage_memory_source`、`anda_brain_remember_conversation`、`anda_brain_recall_memory`、`anda_brain_run_maintenance`、`anda_brain_execute_kip_readonly` 等记忆工具。

### 集成

1. 记忆：发送对话以进行记忆编码
```bash
curl -sX POST https://your-brain-host/v1/my_space_001/formation \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [
      {"role": "user", "content": "I work at Acme Corp as a senior engineer."},
      {"role": "assistant", "content": "Nice to meet you! Noted that you are a senior engineer at Acme Corp."}
    ],
    "context": {"counterparty": "user_123", "agent": "onboarding_bot"},
    "timestamp": "2026-03-09T10:30:00.000Z"
  }'
```

2. 召回：在响应前查询记忆
```bash
curl -sX POST https://your-brain-host/v1/my_space_001/recall \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "query": "Where does this user work and what is their role?",
    "context": {"counterparty": "user_123"}
  }'
```

### CLI（anda-cli）

完整 CLI 用法请参考 [anda-cli/README.md](https://github.com/ldclabs/anda-brain/tree/main/anda-cli)。

CLI 保留结构化结果，并在结果报告执行失败时返回非零退出码。批处理清单绑定目标服务、
Space 和 shard；文件内容变化后会重新提交，`submitted` 仅表示服务端已接受入队。
帮助文本不会显示环境变量中的密钥。Wiki 导出文件可直接重新导入；使用
`wiki commit --input` 时，所有文档字段必须写在该 JSON 中。

```bash
# 提交记忆生成（JSON 消息）
anda-cli --space-id my_space --token $TOKEN formation \
  --messages '[{"role":"user","content":"你好"},{"role":"assistant","content":"你好！"}]'

# 提交记忆生成（纯文本）
anda-cli --space-id my_space --token $TOKEN formation \
  --messages '这是一段纯文本记忆。'

# 从文件提交记忆生成（JSON 或纯文本）
anda-cli --space-id my_space --token $TOKEN formation \
  --file ./message.txt

# 通过 stdin 管道输入纯文本
echo '来自 stdin 的纯文本记忆' | \
  anda-cli --space-id my_space --token $TOKEN formation
```

## 为什么起名“Brain (大脑)”？

这个名字代表了我们的设计理念。我们构建的不是一个静态的数据库，而是一个人工认知器官。正如人类的大脑一样，这个系统在白天**编码 (Encode)** 体验，在夜间**巩固 (Consolidate)** 知识，醒来后以更精确的认知**召回 (Recall)** 记忆。

这背后是一个**数据飞轮**：业务 Agent 在日常工作中产生对话 → Brain 自动编码为结构化知识 → 睡眠周期进行巩固、去重、关联 → 更丰富的知识让 Agent 的决策更精准 → 更好的决策产生更高质量的新数据。这个闭环运转得越久，认知能力越强，竞争对手越难追赶。

**是时候让你的 AI 能睡一觉了。**

## 延伸阅读

- [AI 的记忆必须能睡眠——而只有知识图谱能让它入睡](https://github.com/ldclabs/anda-brain/blob/main/posts/AI_Memory_Must_Sleep_cn.md)
- [Claude Code 记忆系统深度解读：AI 是如何「记住」你的？](https://github.com/ldclabs/anda-brain/blob/main/posts/Claude_Code_Memory_Research_cn.md)
- [当 AI 学会本体建模：Anda Brain 让企业“长”出自己的智能大脑](https://github.com/ldclabs/anda-brain/blob/main/posts/Enterprise_AI_Brain_cn.md)
- [AI 的第二种训练——用词元铸造记忆图谱](https://github.com/ldclabs/anda-brain/blob/main/posts/Tokens_Anda_Brain_cn.md)
- [将公司构建为一个智能体，需要一颗“大脑”](https://github.com/ldclabs/anda-brain/blob/main/posts/Company_Built_As_Intelligence_cn.md)
- [从“编译知识”到“铸造大脑”——Anda Brain 回应 Karpathy 的 “LLM Knowledge Bases”](https://github.com/ldclabs/anda-brain/blob/main/posts/LLM_Knowledge_Bases_cn.md)

## 许可证

版权所有 © LDC Labs

基于 Apache-2.0 许可证授权。

效用校准运行时已增加图外 Recall 交付收据、独立贡献归因、Concept utility 原子有界校准及同优先级
可选排序。检索频率和模型自述不会加分；方法须显式配置参数并经校准审查，更正会
停用相关排序信号而不改写历史收据。详见[记忆效用](anda_brain/UTILITY_RUNTIME_cn.md)。

## 可信宿主记忆产品合同（v0.12.1）

v0.12.1 的原生合同为可信嵌入宿主提供来源记录视图、可确认的更正、带来源栅栏的停用/删除、收件人订阅及学习准备度。详见 [Rust 产品合同](anda_brain/API_cn.md#trusted-host-memory-product-contracts)，这些接口与普通自然语言模型工具分开。
来源引用会与实际捕获的消息或已确认更正回执核对；直接图谱擦除也会清理受影响的产品预览副本。
