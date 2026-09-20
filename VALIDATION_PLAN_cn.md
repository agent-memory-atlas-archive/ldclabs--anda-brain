# 运行观测与真实效果验收方案

**[English](VALIDATION_PLAN.md) | [中文](VALIDATION_PLAN_cn.md)**

状态：**0.12.0 的待实施与待验收方案**，2026-09-20。本文不表示指标已实现、业务
校准已完成或允许开启自动学习。KIP v2 尚未上线，以新建 v2 Space 为基线，不安排
旧数据盘点和迁移；正常重启、驱逐、取消、撤权和确认丢失恢复仍是必须验证的能力。

## 1. 当前基础与责任边界

| 责任方 | 复用内容 | 待交付 |
| --- | --- | --- |
| Brain | [注意力报告](anda_brain/src/attention/mod.rs)、[动作收据](anda_brain/src/action/service.rs)、[后果入口](anda_brain/src/consequence/service.rs)、[运行状态](anda_brain/src/runtime_api/service.rs) | 持久指标身份、有界盘点、阶段计时、成本归属与授权导出 |
| Brain | [业务 HTTP 适配器](anda_brain/src/learning/workflow_http.rs)、[Recall 收据](anda_brain/src/recall_receipt.rs)、utility/semantic/trust 运行时 | 提供评测所需的可信测量/审计接缝，保留原生权限和预算 |
| MIB | [长期学习框架](https://github.com/ldclabs/MIB/blob/main/docs/harness/MIB-Learning-Longitudinal.md)、产品回归、冻结锁与报告重放 | 真实宿主准入、完整尝试级测量、语义/效用/信任实验配置及可验证报告 |
| 业务宿主 / Anda Bot 集成 | 隔离运行及已有 Agent/记忆后端协议 | 真实 executor、独立 observer、计划工厂和提供商用量接线；如实声明支持的条件 |
| 部署方 / 独立审查者 | 现有版本化运行配置 | 冻结合同、审查实测校准、分别开启各能力 |

已核对本地 MIB 的 `scripts/check-brain-product-regression.py`、
`src/mib_runner/learning/benchmark.py`、`docs/harness/MIB-Learning-Longitudinal.md`。
当前产品回归脚本运行的是 fixture，不是 Brain；长期学习报告尚不能证明完整原生学习，
普通 Agent 输出也没有完整的尝试级提供商用量。先补齐这些接线，再判断真实效果是否通过。

不预设需要发布新版 AndaDB。先验证已有公开收据/历史 API；若实际缺少必要字段，
将上游前置拆为独立交付，先发布再消费。禁止使用 sibling path override，也不在
Brain 内重建全面离线评测器。

## 2. 冻结测量合同

拟新增 `anda_brain/src/observability/`，包含版本化事件类型、有界存储、聚合与可选
宿主导出器；通过 `lib.rs`、`runtime_api/config.rs`、`space.rs` 和
`space/attention.rs` 接线，先注册再开始产生事件。这些模块是**计划新增**，尚未实现。

`RuntimeMeasurement` 包含格式、shard、Space instance、原生序号、阶段、操作 ID、
父级/关联引用、事件时间、观测时间、时钟域、结果/原因码、重放身份及可选用量。
不保存提示词、回答、原始 Evidence、用户标识、端点凭据或 token。详细关联引用仅供
可信宿主审计；指标标签只使用有界 stage/status/reason 枚举，不使用 Space、Watch、
actor 或事件 ID，避免泄密与无限标签数量。

事件 ID 由 instance、stage、原生操作身份及收据版本稳定派生。重读或重放同一原生
完成不会增加逻辑完成数；真实网络/模型重试使用独立 call-attempt ID，计入真实费用。
逻辑完成与调用尝试分别统计，不依赖 Nexus 私有幂等键编码。

| 指标 | 定义与可信来源 |
| --- | --- |
| deadline→wake | 定时 Watch 使用 `max(0, 原生持久触发时间 - 规范化 due 时间)`，合法提前匹配不属于时钟错误；无 due 的 delta Watch 单独统计触发事务提交→wake |
| wake→gate | 原生 gate 提交时间减其消费的 wake 创建时间；澄清和后继工作按各自来源分别统计 |
| gate→dispatch | 首次原生分派准入时间减 gate 提交时间；目标确认和独立终结 Outcome 另列阶段 |
| 待办数/最老年龄 | 固定盘点边界下的非终态工作，按有界原因分类；未知分派仍为待办，取消/完成排除 |
| 覆盖滞后 | 同一原生 Space/arm 内，固定目标序号减已证明消费序号；仅有可信序号时间映射时报告时间滞后 |
| 重试/去重/拒绝 | 区分真实尝试与原生回执重放；拒绝的观察只留元数据，不变成可信图事实 |
| 后果消费积压 | 各已配置消费者尚未确认的合格收据索引；排除永久不合格项，单列未决接收 |
| 学习/复审 | 热任务、归档、容量、未完成复审义务与逾期年龄；归档不代表采纳 |
| 成本 | Formation、Recall、Maintenance、语义求值、gate、执行器及观察者的每次实际模型/工具尝试；分列 token、工具调用、耗时和货币 |

进程内耗时用单调时钟；跨重启/服务仅比较同一可信时钟域的时间，保留偏差或 unknown。
实验 business time 不与墙钟混用。缺失或时钟倒退产生的无效测量不是零。
记录 `measurement_started_at`；本次未上线部署不需要补填历史指标。

每笔成本包含 request/attempt 身份、作用域、阶段、来源、token/调用数、可空金额/币种及
`accounting_complete`。累计提供商快照独立保留，不当增量求和；货币换算须固定提供商、
模型、价格表、币种和生效时间。缺少计费或失败调用用量时，不能宣称总成本完整。
本地 tokenizer 估计值与提供商计费用量分列。

## 3. 可恢复采集与有界导出

1. 在 `attention/service.rs`、`attention/semantic/runtime.rs`、
   `action/{gate,native,dispatch}.rs`、`consequence/service.rs`、
   `learning/runtime/{scheduler,catalog,settlement}.rs` 及 utility/trust 提交路径加入采集。
   确认原生结果后才记为已提交；复用返回的原生引用和当前授权读取。
2. 工作开始前保存有界测量发现 intent。原生事务与指标写入不假装原子；指标 ACK 丢失
   按相同事件 ID 恢复，原生提交后进程退出由有界收据盘点补齐。未决 intent 不冒充完成。
3. 拟定默认上限：16 个准入槽、每轮盘点 200 条、单事件 64 KiB、每 shard 最多
   10,000 条排队记录或 64 MiB，先到者生效；已导出指标保留七天。上限和保留期由
   宿主显式配置，序号检查点使用 CAS；暴露缺口、饱和、可选样本丢弃及保留边界。
4. 指标故障不改变 Decision、Outcome、trust version 或权限，也不阻塞已接收的原生
   写入。标记测量不完整并继续有界对账；来源已擦除/不可用时明确记为缺失。
   不为恢复指标而额外保留原始文本。
5. 盘点固定边界并持久化 cursor；部分计数为下界，包含 `complete:false`、`as_of`、
   `scanned`、`next_cursor`。不能把首屏当全部积压；用 generation/revision 约束避免
   新旧工作混算，并披露时钟及扫描滞后。
6. 拟新增可选 `RuntimeStatus.observability`，报告配置、健康和完整度。全 Space 库存及
   时延仅 auditor 可见；普通接收者继续使用原有过滤计数。先提供可信 Rust sink，
   部署指标端点须独立认证，不增加模型写入器或默认公开的指标端点。
7. 固定且版本化直方图桶，拟用秒数：0.01、0.05、0.1、0.5、1、5、15、30、60、120、
   300、900、overflow。报告桶边界及未完成窗口；回归使用 fixture 保留的准确耗时，
   不能将分桶估计冒充准确 P95。导出带窗口/事件身份的绝对聚合值，避免导出器重启时
   重复增加远端计数。

测试放在 `observability/` 和 `space/tests/`：同一提交重复观测、ACK 丢失、两个提交
之间退出、原生失败、旧盘点、多页、时钟倒退、未知分派、成本缺失、擦除/撤权、队列
耗尽、取消/停机排空以及接收者/auditor 隔离。

退出条件：确定性 fixture 可重建每个原生收据恰好一次逻辑完成及所有真实调用，且
不改变分数或权限。同一记录下的主机、存储、负载比较指标关闭/开启：已有 20 Space、
200 Watch、5 秒 tick 基准保持 deadline→wake 的 P95 ≤60 秒，吞吐/时延退化 ≤5%。
这是拟定工程验收门槛，不是生产 SLA；过载和停机单独报告。

## 4. 将 MIB 接到真实宿主

Brain 测量合同稳定后，在 MIB 与业务宿主分别提交接线。复用 `learning/benchmark.py`、
不可变锁和分数重放，为 `tool_workflow.precondition.v1` 接入实际执行器、独立观察者与
计划来源，再允许 `normal` 和隔离 `ungated`。`persistent` 且 `learning:false` 的宿主
继续拒绝，不能改名冒充正常学习组。`ungated` 仅在隔离原生 trial 内放宽推荐 standing；
每组仍执行当前权限、fence、预算和独立后果检查。

扩展 MIB 报告、schema 和验证器，将每个 Decision、Attempt、Outcome、Trial、Evaluation
和提供商调用关联到准确 run/revision。重放真实工具日志与用量，不信任成功标签或能力
声明。缺失计量保留 unknown/insufficient。训练、验证、世界 seed、隐藏标签及审计材料
不得进入业务 prompt、Formation 或候选生成。可在安装运行身份之前复制不可变事实基线，
不能 fork 已配置的 attention/learning journal。

在 MIB 的 `profiles/`、`schemas/`、`src/mib_runner/`、`tests/` 增加独立实验配置和
验证器。语义、utility、trust 的新命令须先实现并文档化；既有长期学习命令并不自动
评估这三类能力。

## 5. 预注册各自独立的效果门槛

验证前冻结：代码提交、profile/合同摘要、模型部署及 prompt、tokenizer、工具、环境、
独立观察者身份、事实基线、训练/校准/验证划分、全部 seed/条件、预算、缺失规则、
样本量计算与停止规则。只在训练/校准集调参，进入保留验证集前固定参数；重要 pin
变化须重新校准并使用新的保留验证运行。

以下是**拟定首版发布指标**，须在查看验证结果前批准并冻结；它们不是生产隐含默认值：

| 能力 | 对照 | 通过条件 |
| --- | --- | --- |
| 学习 | 相同业务模型/工具/预算的 normal、no_memory、隔离 ungated；覆盖早期成功、负迁移、漂移与撤销 | normal 相对 no_memory 的任务成功率增益，配对置信下界 ≥5 个百分点；不安全率增加上界 ≤1 个百分点；无撤销后使用；各分层全部报告 |
| 语义 Watch | 独立盲标事件页，含文本/混合/空页/未知条件，固定模型/prompt | 错误 silence 率上界 ≤1%；delta 精确率/召回率及可判定页完成率下界 ≥95%；unknown/漏项不能推进覆盖；另报弃答与时延 |
| utility | 同优先级内校准排序开/关；单一贡献和配对修订分别验证 | 任务成功率增益的配对下界 >0；不安全率增加上界 ≤1 个百分点；retrieved-only、bundle、重放不加分；必要约束不丢失 |
| contextual trust | 冻结原生权重与限定域校准权重，使用独立保留事实根 | 限定域 BELIEF 决策错误率降低的下界 >0；无依据 accepted-belief 比例增加上界 ≤1 个百分点；无全局/其他域影响或 confidence 改写；验证回退及漂移 |
| 成本/权限 | 每次实际提供商/工具尝试及准确原生来源 | 符合预算资格的原生 success 具有完整实测用量；无虚构零成本、越权、凭据泄露或重复外部效果 |

语义测试的错误 silence 分母为所有确实包含合格匹配的已标注 Watch/截止窗口，不能只
统计模型实际发出 silence 的窗口。delta 精确率以发出的阳性为分母，召回率以全部
标注阳性为分母；完成率以独立标注为可判定的页面为分母，预期 unknown 单独分层且
不得推进覆盖。空分母为 insufficient，不是满分；每个计划窗口及拒绝都须报告。

预注册效果/安全假设的总体 alpha 为 0.05，事先固定多重比较修正规则和主假设；针对
声明的最小效果，功效目标 ≥80%。在独立 pilot 估计方差与簇大小后，锁定样本量。
不存在适用于所有部署的固定样本数。按独立任务/事实根簇比较或重采样，不把重复事件、
Evidence 副本当独立样本。计划分母保留失败、超时、缺失运行，不剔除不利分层；
严重权限/安全事件立即失败，证据不足不能通过。

示例：299 个独立案例零错误时，未经多重比较修正的单侧 95% 错误率上界低于 1%。
若修正后尾部概率为 `a`，这个零错误上界至少需要 `ceil(log(a)/log(0.99))` 个独立案例；
存在簇相关或观察到错误时，使用预注册的区间方法。原生 trust 每提案最多 32 个样本、
utility 的原生分组上限保持不变；更大的部署验证使用独立实验，不能绕过运行上限或
重用已消费的事实根。

当前 trust 方法只处理二元事实准确率，不做预测概率校准。分别报告 BELIEF 的
accepted/rejected/insufficient 和覆盖/弃答，不能靠全部拒答制造低错误率。
trust 与 utility 对照预注册的允许覆盖损失最多为 1 个百分点；不对 Assertion
confidence 计算预测概率 Brier 分数。

## 6. 实施顺序与交付物

| 顺序 | 责任方与交付物 | 退出条件 |
| --- | --- | --- |
| 1 | Brain：事件/时钟/成本 schema、配置与故障测试 | 稳定身份、有界存储、明确 unknown 和隐私规则 |
| 2 | Brain：采集点、盘点、状态和宿主导出 | 恢复/隐私/负载验收通过；同步双语 API/指南/技能 |
| 3 | 业务宿主 + MIB：真实绑定及尝试级成本 | 无模型集成测试证明真实原生来源，拒绝不完整能力 |
| 4 | MIB：四类冻结实验配置与重放验证器 | 独立 pilot 确定样本量，锁包含所有标准，机制报告仍为 not_evaluated |
| 5 | 独立评测方：真实保留集运行 | 可复现报告/锁/进度/受控原始材料，各门槛单列 pass/fail/insufficient、排除项与不确定性 |
| 6 | 部署方 + 审查者：校准导入与有限放量 | 批准准确配置摘要，验证各能力独立开关和恢复演练 |

已有命令如下。按标注仓库运行；真实提供商测试需要明确费用预算和实际配置的端点：

```sh
# Brain 工程检查
cargo fmt --check
cargo clippy -p anda_brain --all-targets --all-features -- -D warnings
RUST_MIN_STACK=16777216 cargo test -p anda_brain --all-features
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features wiki
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features mcp

# MIB：fixture/协议验证，不代表 Brain 真实效果
uv sync --extra test
uv run python scripts/check-brain-product-regression.py --output-dir /private/tmp/brain-product-check
uv run pytest tests/test_product_regression.py

# MIB：完成真实绑定、成本和报告验证后再运行。
# 以 examples/learning-longitudinal/experiment.json 为基础，审查并替换为真实端点。
uv run python -m mib_runner learning-benchmark /absolute/private/experiment.json --output /absolute/private/run.report.json
uv run python -m mib_runner verify-score /absolute/private/run.report.json
```

每次运行使用新的 evaluator 私有输出路径。`--resume-lock` 保留冻结任务并新建隔离 run，
不允许盲目重放原 run 中未知的外部动作。`verify-score` 只证明报告内部可重放一致性，
业务发布门槛须由新增 profile 验证器逐项检查。

校准交付物映射到现有 `LearningCalibration`、`UtilityCalibration`、`TrustCalibration`，
包含准确运行摘要、独立审查者和可重放材料引用；语义验收固定准确 evaluator 配置。
先上线只读观测，再仅产提案且关闭自动应用，最后启用一个通过验收的任务/作用域。
拟定灰度观察期为七天且达到锁定的最小独立暴露数，以更晚满足者为准。
出现越权、测量断裂或漂移时停止新增自动工作，对账未知分派、排空写入并保留原生历史。
trust 回退追加受控恢复版本，不能改写 Assertion 或删除治理证据制造健康结果。
