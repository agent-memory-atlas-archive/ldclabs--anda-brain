# 学习运行时

**[English](LEARNING_RUNTIME.md) | [中文](LEARNING_RUNTIME_cn.md)**

学习运行时已接通原生配对控制器的后台推进，提供 `workflow_http_v1` 业务宿主适配器、
持久历史索引、终态归档及独立安全信号消费者。当前只支持 `tool_workflow.precondition.v1` 任务族。编译通过、
配置就绪和任务完成均不能证明学习效果；MIB 的真实效果验收仍是独立发布条件。

## 启用

完整服务使用 `--features mcp,wiki,learning`；精简库启用 `learning` 即可。运行时启动配置中的 Space 可新增 `learning` 对象，
与 inbox 配置并存，见 [learning.runtime.example.json](learning.runtime.example.json)。示例中的自动开关和校准批准均关闭，摘要、材料、服务地址及
阈值必须替换为实际经过审查的数据；示例阈值不代表已完成生产校准。

三个环境变量分别提供执行器、独立观察者、计划来源的凭据，必须互不相同。服务
地址只来自可信启动配置，除 loopback 外须使用 HTTPS，禁止跟随重定向。普通
HTTP/MCP 请求不能注册执行器或冻结 cohort，凭据不会进入记忆、模型上下文或收据。

启动及每轮自动试验都会验证实际执行器能力、新鲜观察者身份及当前原生权限。
`bootstrap` 使用运行时的可恢复授权逻辑，不会在重启时恢复已撤销的权限，也不会给
候选 SkillRevision 自动授予可执行权限。

`LearningCalibration` 包含不同的训练/验证清单、实际报告、审查者、环境及 `calibration_contract(&registration)` 返回的合同摘要。自动
trial/review 需明确批准这些固定输入；模型、工具、预算、观察语义或阈值变化都会
改变合同。该检查验证宿主的审查声明与材料一致性，不能替代 MIB 实测质量判断；
测试中的审批和确定性模型端点只用于验证机制。

Rust 宿主可在 `MemoryRuntimeBindings.spaces[space].learning` 安装 `LearningBindings`，
或在自动工作前调用 `LearningRuntime::install_bindings(AuthContext::system(), bindings)`。
回调实现 `LearningExecutor`、`LearningObserver` 和 `LearningPlanFactory`。默认 executor readiness probe 会拒绝自动
装配，需宿主显式验证重置、查询、预算与取消路径。registration 在该实例中保持
不可变；冷启动重新安装相同代码绑定，不能复制运行身份到另一个 Space。

## 调度与状态

注意力 tick 为每个 Space 最多启动一个运行时持有的学习任务，Watch 扫描无需等待业务
I/O。每轮推进一个热任务、接收一个独立观测、扫描八个历史复审槽及八个安全收据槽，
最多报名一个获准复审。游标持久化。任务必须来自已注册计划工厂的冻结清单，Watch
触发本身不授予报名资格。新分派保留来源、事件及可选 gate/触发引用；原生 rationale
中的 JSON 保留相同来源，旧记录可无此字段。

`automation.trials`、`reviews`、`archive`、`safety` 分别控制试验、复审、归档和安全消费。宿主 `automatic=false` 禁止调度工作。
registration 暂停后不会开启新试验或比较结论；显式允许的安全恢复与终态归档仍可
处理既有义务。回调最多 30 秒，实际执行受剩余原生租约和预算约束；关闭会取消
回调并等待持久写入，发送结果不确定时保留原身份对账。

`GET /v1/{space_id}/runtime/status` 与 `anda_brain_get_runtime_status` 的 `learning`
对象含 `compiled`、`registered`、`registration_enabled`、`bindings_ready`、
`automatic_allowed`、`running`、`automation`、`blocked_reasons`，区分编译、注册、注册开关、实际绑定及自动许可。只有明确映射的
auditor 获得容量和上轮推进信息，其他调用方这两项为 null。推进计数不代表正向
verdict。Maintenance 不内联执行业务试验：旧计数保留零，`skills.runtime` 显示独立
调度状态，并明确说明本轮 Maintenance 没有执行学习比较。

## 业务宿主协议

三个 base URL 均以 `/` 结尾，响应使用 JSON，最多 256 KiB。HTTP 错误、身份变化、
截断日志、超时或未知发送结果不能推断为成功或确实未执行。具体结构见 [workflow_http.rs](src/learning/workflow_http.rs)；HTTP 适配器属于正式代码，loopback 业务服务属于测试代码。

| 通道 | 路径 | 合同 |
| --- | --- | --- |
| executor | `GET capabilities` | `WorkflowCapabilities`：准确身份、校准 pin、隔离重置、幂等、权威状态、fence/期限约束、取消及完整测量 |
| executor | `POST reset` | 冻结分派头 → `WorkflowReset`；在隔离世界/会话中复现准确任务和初始状态摘要 |
| executor | `POST decide` | 公开任务、冻结实验组程序或 null、真实回复和剩余 token 预算 → `WorkflowChoice`；使用固定模型和事实记忆 |
| executor | `POST inspect`, `prepare`, `commit` | 分派头、请求摘要、序号及稳定幂等键 → `WorkflowReply`；目标端执行 pins、fence、期限及工具预算检查 |
| executor | `POST finish` | 封存完整真实日志；仅确认回复不会创建 Outcome |
| executor | `POST cancel` | 幂等停止未完成工作；即使客户端失联仍执行租约到期约束 |
| executor | `POST status` | 相同分派头 → `WorkflowStatus`，请求摘要必须匹配 |
| observer | `GET identity` | 单独认证且准确匹配的 `ObserverControl` |
| observer | `POST journal` | 相同分派头 → `WorkflowJournal` 或 null；读取真实持久化有序状态变化和全部模型用量，包括失败/重试 |
| source | `POST enrollment` | 实例、来源 pin、最近接纳的事件或复审义务 → 冻结 `LearningEnrollment` 或 null |

分派头包含真实原生身份、fence/deadline、固定 PairCase、环境/模型/事实记忆/工具与
预算 pins，属于宿主信封。模型只接收业务任务、真实工具回复、冻结 procedure 及其
上下文预算；不得传入 seed、实验组标签、隐藏状态、评估成绩或验证日志。

观察者根据日志顺序及操作前后状态独立复核，计入失败提交、禁止的 prepare 和实际
耗时/token；缺失用量保留 unknown。原始日志最多 16 KiB，在提交独立观测前进入
受保护的宿主 journal，可通过 `LearningRuntime::observation_replay(job_id, dispatch_id)` 重放，不进入 Recall/HTTP/MCP。
只有既有 LearningRuntime 写原生 Outcome，cutoff 和幂等恢复规则不变。

业务宿主负责真实操作、日志和模型调用，适配器不会凭空提供业务系统。装配要求
服务通过实际探测；磁盘 loopback 测试证明通信、重置与持久化机制，不证明已部署
外部业务系统或获得真实模型收益。

## 归档、复审与安全

`maximum_jobs` 现为 1–32 的热工作集容量。`jobs()` 只返回热任务，`jobs_page` 按
固定数字槽分页查询历史并明确报告完成度。`report` 跨冷热查询；`reviews_page`
使用独立持久化复审索引及有界热目录，旧 `reviews()` 超过一页时会要求分页。

归档前核查全部待提交 intent、终结收据、原生记录及 acquisition 引用。未知外部
效果、缺失后果或未解决安全记录保留热槽并显式背压。已由独立终结后果关闭的原生
分派可重新取得租约完成过期任务的记账，不会重发业务操作；未知后果不能走此路径。

归档释放热目录并保存经核验的不可变终态快照，原 job 地址保留后续复审链接及安全
检查点。`archive_replay` 返回原快照，当前 `report` 可能包含后续撤销；原生证据不
删除，ID 不可用不同输入复用。Attempt、verdict 与复审索引均能跨驱逐和重启恢复。

存储策略独立限制保留身份与宿主 journal 逻辑预留，默认 4096 条身份、每条 64 MiB，
单对象上限 8 MiB，可显式下调。该预留覆盖编排和有界重放材料，不代表实际磁盘
占用，也不是 Nexus 证据保留策略。容量不足不消耗复审义务；不自动删除证据、
压缩历史或放弃未知外部效果。

v1 首次迁移导入原有最多 32 个任务，保留原 ID、实例、配置和重放记录，并可恢复地
发布新目录。之后不依赖对象存储 listing 顺序。报名、归档使用 CAS 与精确读回，
发布中断可恢复；仍要求每 shard 单活动宿主和支持条件写入的存储。

没有获准的新 cohort 时，复审保持逾期，当前推荐资格到期。只有持久子任务承接
义务后才确认旧计时提醒；报名失败或未激活即过期的子任务使原义务重新出现。
认证的迟到/冲突安全收据仍可进入独立消费者，用新鲜身份完成原生撤销后才推进
检查点，不修改旧试验成绩。`safety_pending` 表示尚未解决，完成后保留 `safety_evaluation_ref`。
后续信号可由仍然有效的同一撤销记录覆盖；重新进入试验后，信号定位到当前 revision
的控制器任务，不覆盖旧信号或旧 verdict。

## 验收边界

机制测试覆盖连续 34 个原生终结试验、冷热重放、归档后采纳依据/复审/撤销、无新
cohort 与容量故障、v1 迁移、归档 ACK/检查点故障、真实 HTTP 启动、磁盘重置及
日志、租约过期恢复。原有固定 cutoff、基线未知、treatment 缺失和取消测试继续保留。

MIB 还须固定训练/验证划分、环境、模型、预算、基线和缺失规则，实测预声明的效应
下界与失败率门槛，并验证后续漂移及撤销。Brain 未重建离线评测器，不宣称已
获得真实学习改善。
