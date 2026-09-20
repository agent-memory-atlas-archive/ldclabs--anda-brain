# 文本 Watch 运行时

**[English](SEMANTIC_WATCH_RUNTIME.md) | [中文](SEMANTIC_WATCH_RUNTIME_cn.md)**

语义 Watch 运行时已接入 Nexus 0.13.1 的 prepare/read/commit 接口。文本及混合条件 Watch 必须
显式安装 evaluator：模型逐项判断事件，Nexus 独立重验覆盖、权限、basis、页面摘要、
Watch 版本和布防代次。未配置时保持 blocked；模型完成调用或自称 complete 不证明覆盖。

## 启动装配

在加载 Space 前，通过 `BRAIN_RUNTIME_CONFIG.spaces.<id>.semantic` 安装。
[配置模板](semantic.runtime.example.json)默认关闭自动模型调用。替换精确模型 ID、
完整 Chat Completions endpoint，并提供环境变量密钥；使用固定版本的模型部署，
响应 `model` 必须与配置完全一致；实际 HTTP 客户端发送前还会核对请求 pin，
修改配置副本不能重新标记旧客户端/endpoint。不会回退到 BYOK、Recall、Formation 或 Maintenance
的模型；精简库可用，不依赖 `learning` feature。

编译适配器使用单次非流式 JSON 请求、固定提示词、完整页面、temperature=0 和输出上限，
不给模型工具。除 loopback 外要求 HTTPS，禁止跳转。提供商报错、拒答、模型替换、
非 stop 结束、非法/额外字段、漏项/重复、pin 不符、输出超限均不能推进覆盖。
密钥只在启动时解析，不进入 pin 或持久化材料。受信任 Rust 宿主也可安装
`SemanticBindings` / `SemanticEvaluator`，但必须使用已经计数的完整请求，不得添加隐藏
历史、替换模型或在旧 pin 下更换实现。

`principal` 是独立于 API 调用者和后果观察者的宿主服务身份；同时安装 action/inbox 时，
必须与布防用的 attention controller 一致。`bootstrap:true` 可一次性配置
`read/read_history/read_governance_history/create/update/derive/maintain`；
重启不能恢复已撤销的授权。`bootstrap:false` 需宿主自行预置。模型不能提供身份、
修改 pin、重新布防或执行动作。触发后的 wake 仍经过动作 gate 与当前执行权限检查。

## 预算和调度

| 限制 | 默认值 | 允许最大值 |
| --- | ---: | ---: |
| 每轮 Watch/页面数 | 2 | 4 |
| 每页原生变更信封数 | 32 | 200 |
| 每页候选变更数 | 32 | 64 |
| 每次模型请求输入 token | 16,384 | 65,536 |
| 每次模型请求输出 token | 4,096 | 16,384 |
| 每轮累计输入 token | 32,768 | 262,144 |
| 模型回调时间 | 15 秒 | 60 秒 |
| 模型工作阶段时间 | 30 秒 | 60 秒 |
| 重试间隔 | 60 秒 | 1 小时 |
| 每页模型尝试次数 | 2 | 4 |

这些数字是资源上限，不是经验校准阈值。输入按完整序列化 JSON 请求、输出按返回正文
使用 `o200k_base@tiktoken-rs-0.12.0` 计数，不冒称提供商计费/隐藏模板的完整核算。HTTP 响应最多
512 KiB，模型 JSON 正文最多 256 KiB。候选数或输入超限时不截断页面。空授权候选页
无需模型调用，但仍必须由 Nexus 证明覆盖。任何一项 unknown 都会阻止整页推进，
即使另一项 match；silence 必须证明原生固定截止序号之前没有匹配。截止前准备的页面
不会因为模型在截止后完成而自动扩大时间范围。

每 Space 单飞，单宿主目录最多并行 4 个语义 pass，与 HTTP/MCP 普通模型并发预算独立。
结构化扫描和 wake 处理不等待语义模型 I/O；语义调度有独立持久化游标。
自动运行同时要求宿主、attention registration 和 `contract.automatic` 开关；未配置或
关闭时不自动调用模型。模型受超时限制，原生/存储写入必须完整排空，慢存储可能延长
整轮耗时。关闭或驱逐不会中途取消已接收的持久化工作。

## 恢复与宿主接口

| 可信 Rust API | 用途 |
| --- | --- |
| `Space::attention().semantic()` | 获取已安装的语义运行时 |
| `SemanticRuntime::run_once()` | 在 registration 已启用时执行一轮，可保持自动模型开关关闭 |
| `status()` / `progress(watch_ref)` | 查看配置、上轮统计或 Watch 保留的回执引用 |
| `retry(watch_ref)` | 显式重试，保持原页面及 evaluator，不填补观察空窗 |
| `AttentionRuntime::configuration()` | 原生配置及 CAS 版本 |
| `reconfigure_evaluator(expected_version)` | 显式更新 evaluator pin，不自动布防 |
| `AttentionRuntime::set_enabled(false)` | 停止后续调度，已有工作完成持久化或保留未决状态 |

已有 Space 升级时先安装新的启动绑定，再读取 `configuration()`，用其原生 CAS 版本
显式调用 `reconfigure_evaluator`；逐项审查受影响 Watch 后，按当前元素版本重新布防。
配置更新不改写旧代次，不证明观察空窗。原生 attention pins 共享 configuration basis，
evaluator 变更也可能要求复核结构化 Watch 与现有 action 工作。该方法不能迁移 policy、
action binding 或 scope。basis/history 缺口保持 blocked，重试不能替代缺失证据。

准备键绑定 scope/pin/Watch 版本/代次。提交前，完整核验结果写入受每个页面来源控制的
原生 Artifact；Brain 图外 CAS 日志只保存引用、版本、计数和固定状态原因。
丢失 ACK 或提交后的检查点失败时，使用同一响应与 evaluation key 重放，不重复触发，
不再次调用模型。来源擦除与当前权限同样约束重放材料；Brain 目录没有原文副本。
调用方丢弃 future 不会取消已接收的提交。

HTTP/MCP 状态新增 `semantic_attention`：配置、自动运行资格、运行状态、pin 和固定原因。
只有配置为 auditor 的调用者可读取全局上轮计数，普通调用者得到 `last_pass:null`；
这些计数不是完整认知/变化流清单。模型侧 `memory_runtime/status` 仍为只读宿主状态，
未新增模型/HTTP/MCP 配置或求值写接口。

## 效果边界

原生故障、生命周期、预算和 localhost 提供商测试验证机制，不证明真实语义准确率。
生产启用 silence 提醒前，需要在 MIB 对固定模型/提示词及代表性文本、混合条件实测。
固定名称与 pin 记录部署合同，不能独立证明远端权重或提供商行为。语义判断不改变
Assertion confidence、BELIEF、Skill standing、utility 或执行权限。
