# 记忆效用

**[English](UTILITY_RUNTIME.md) | [中文](UTILITY_RUNTIME_cn.md)**

效用运行时区分“交付、使用、实际贡献”，只在独立证据及预先固定的校准方法允许时更新
Concept 的 utility。检索次数、引用、点赞、模型自述或单纯任务成功均不加分；
Assertion confidence、BELIEF、Skill standing 和执行权限不因 utility 改变。

## 交付收据

每个 Space 均安装图外交付收据。预算 Recall 保留精确 packet 摘要、交付条目摘要、
真实引用/版本、内容 pins、coverage、basis 和生效预算；不声明语义完整或执行许可。
收据写入由运行时持有并读回，Recall 不修改认知图。

结构化 Recall 返回可选 `recall_receipt:{id,digest,scope}` 句柄；普通/直接 agent 调用仍返回 conversation，可信宿主
用 `Space::recall_receipts().for_conversation(id)` 查询。旧自然语言答案记录真实检索轨迹，但标为 legacy_trace_only，不能靠
自由文本证明完整交付，因此不进入自动贡献归因。空或预算不足的 packet 同样不
具备归因资格，收据元数据不会形成额外的记忆内容通道。
收据的 space_instance 标识交付账本，OutcomeInput 仍须使用实际 action/learning
实例标识，不能用交付账本实例替代。

动作 gate 为实际有界上下文签发收据，也可由可信 policy 显式绑定此前 Recall 句柄。
所有 used/applied 引用必须属于该次交付、语义内容未变化且当前仍可读。原生
Decision 的 rationale JSON 保留句柄，used_refs/applied_revisions 仍是原生字段；
澄清子任务为实际使用的父问题生成独立的新收据。

## 归因

消费者沿 **Outcome → Attempt → Decision → used_refs/applied_revisions** 链条读取实际原生记录及创建事务 Principal，不能用 asserted_by 等
语义属性冒充观察者。只检索未使用不加分，多项共同使用且不能分离时记录 bundle，
不平均分摊或复制效果。非 Concept 只留审计，不硬写 MnemonicState，也不为计数
制造 Experience/Insight 副本。

当前支持两种编译内方法：

| 方法 | 依据 |
| --- | --- |
| `single_contribution_v1` | 恰好使用一项记忆；合格独立观察者提供有界 `ContributionWitness`，与实际 Attempt、Decision、交付收据及记忆 pin 一致 |
| `paired_revision_v1` | 复用已配置学习控制器的冻结、绑定修订的原生 Trial/Evaluation 及完整重放证据；试验中只改变一个候选修订，事实记忆、模型、工具和预算相同 |

`paired_revision_v1` 需要 `learning` feature，复用原生比较及当前证据复核，
只支持 `tool_workflow.precondition.v1`，不新造 evaluator 或
猜测基线。未知/缺失基线不变成改善；旧修订的功劳不转移给新 current_revision，
已撤销或依赖未验证的程序不参与 utility 排序。

### 独立见证

已获授权的观察者调用 `POST /v1/{space_id}/outcomes`，提供
`utility:{witness:ContributionWitness}` 或 `utility:{witness_ref:"E-…"}`。
内联见证随认证后的原生 Outcome Evidence 保存，无需观察者自行编写 KIP；引用形式
必须确由同一独立 Principal 写入，且在 Attempt 之后、Outcome 之前。两种形式只能
选一种。普通模型/MCP 工具不能注册观察者或写此后果。

```typescript
type ContributionWitness = {
  format: "anda-brain:single-contribution-v1";
  contract_digest: string;
  sampling_unit: string;
  attempt_ref: string;
  decision_ref: string;
  recall_receipt: { id: string; digest: string; scope: {
    space_id: string; space_instance: string;
  }};
  target: { id: string; version: number; content_digest: string };
  effect: number; lower_bound: number; upper_bound: number;
  confidence: number;
  isolated_contribution: boolean;
};
```

效应及区间限定在 [-1,1]，区间包含效应，confidence 严格小于 1。sampling_unit 必须
来自已注册仪器定义的实际独立业务事件/根；重复副本或后果不能再次加分，也不能换
一个目标重新使用。宿主须核查仪器的物理独立性与因果测量质量，签名不自动证明
实测质量；测试见证只用于机制验证。

## 配置

启动字段 `spaces.<id>.utility: UtilityConfig` 或 Rust `SpaceRuntimeBindings.utility`
安装绑定；示例见 [utility.runtime.example.json](utility.runtime.example.json)。方法明确版本及观察者、任务族、
指标、窗口、环境、工具 pins；环境须涵盖仪器所依赖的模型/上下文假设。不同可比范围
不混合统计。模板默认关闭应用，不能冒充完成生产校准。

`step_cap`、`minimum_independent_samples`、`gain`、`minimum_confidence` 及可选
`initial_utility` 无经验默认值，必须显式提供；合同摘要由 `UtilityConfig::contract_digest()`
产生。校准材料包含精确合同摘要、审查者、
批准及材料；缺参数或批准时拒绝 apply:true，只保留建议/不更新收据。automatic、apply、
rank 分别控制调度、数值更新和排序，隔离宿主的自动开关仍优先。

单一贡献方法按稳定原生引用顺序选择固定数量的未消费独立单位，使用平均效应、区间
包络和保守的 union-bound confidence。配对模式一次使用一个完整原生比较，以固定
cohort 和双侧 Hoeffding 置信度判断效应方向。阈值改变须重新匹配校准，不能择取有利窗口。

对合格样本组采用以下公式：

```text
delta = clip(gain × mean_effect, -step_cap, step_cap)
u_new = clamp(u_old + delta, 0, 1)
```

收据保留旧/新值、delta、方法参数及摘要、纳入/排除后果、证据、不确定性、独立样本数
和前序收据。没有既有校准的起点值明确标为准入假设。缺失、未知、争议、失效、不能分离
或样本不足均保留当前 utility，并记录原因。

## 持久化与排序

消费者先保存完整 intent。原生收据、校准 Activity、Concept utility 和幂等键在同一
MUTATE 事务提交。原生带 CAS 的 no-op 核对输入版本，不更改它们的内容或版本。
派生记忆在同事务保留原有 DependencyBasis，不能借校准创造依赖批准或改变 authority。

检查点丢失先恢复同一原生事务，再确认样本消费；CAS 冲突重新读取并使用新尝试代次。
迟到/更正审计与原 Outcome 分开。认证更正立即停用相关排序信号，生成新收据，不改写
旧收据，也不自行扣除未经验证的数值。存储/读取故障明确报告并保留重试事项。

注意力调度器每轮独立推进最多八个后果索引槽和一个目标，数字目录跨重启保存。目标上限 4096，
每目标最多 64 个待处理样本，单事务最多 512 个输入 guard，私有 journal 单对象上限
8 MiB；超限显式背压。观测索引仍保留原来的一百万索引快照限制。已用样本退出热集合，
独立单位标记及原生历史继续保留。

预算 Recall 只在同优先级内采用经核验的 utility；必要约束、警告、程序状态及原生
不确定性仍优先。排序额外读取最多 500ms，失败时回退原顺序。没有当前有效校准收据
的裸 utility 数值不参与排序，packet 仍不授予语义完整性或执行许可。

运行状态新增 utility 配置、校准、自动、应用及排序标志与恢复原因，不暴露私有校准
材料。可信 Rust 接口为 `Space::utility()` 上的 `enqueue`、`evaluate`、`rank`、`status`；
模型没有设置 utility 的工具。

效用机制测试不代表真实记忆收益。生产参数和观察质量仍需独立实测与审查，MIB 效果
门槛继续作为独立发布条件。
