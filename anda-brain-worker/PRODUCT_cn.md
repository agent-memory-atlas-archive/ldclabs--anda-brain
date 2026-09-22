# 受信宿主记忆产品契约

[English](PRODUCT.md) · [Worker API](README.md) · [移植核对](PORTING_AUDIT.md)

这些方法是供受信嵌入宿主调用的 Durable Object RPC，不是公开 HTTP 路由或模型工具。
宿主必须验证用户身份并提供原生 `AuthContext`；禁止从用户或模型请求体反序列化该参数。
HTTP API Key、语义 actor 和原生 Principal 是不同身份。每次产品调用检查当前原生读取权限，
原生写入另行检查对应操作权限；适配器不会创建 Grant 或 ActorBinding。

## 记录与来源

- `productRecords(auth, before?, limit?)`：按新到旧返回 Assertion 记录；每页最多 50 条，
  默认 20 条。`next_cursor` 是排他的数字 id 上界。有下一页或记录不可读时 `complete=false`。
- `productRecord(auth, assertionId)`：返回 Proposition、语义 actor、立场、认识状态、
  存储状态、版本、有效时间和来源引用。Proposition 存在本身不表示事实为真。
- `productSource(auth, evidenceId)`：返回当前来源引用和 payload 摘要；摘要不是原文，
  原文必须经授权读取 Evidence。来源缺失、已清除、外置或未注册的记录不能通过本接口修改。
- `productCorrectionSource(auth, source)`：只有调用者的已确认更正与当前 Evidence、
  操作绑定、payload 摘要均匹配，才返回更正文案。

Formation 在调用模型前注册 `formation:sha256:…` 观察身份。默认来源是该观察本身；
若有 `context.source`，同时登记 `source:<contentDigest>` 父来源。封禁该父来源也会阻止
同一线程/渠道的后续观察进入记忆。受信宿主可通过
`formMemory(env, brain, input, sourceIdentity)` 指定 `{key, parents?}`：key 非空，
不含控制字符，至多 512 UTF-8 字节，父来源最多 16 个。这些身份只决定记忆准入，不代表
事实、权限或整个请求的 exactly-once 语义；变更预览会显示被排除的来源。

## 审阅式修改与恢复

`productPrepare(auth, {operation_id, record_id, expected_revision, kind, new_value?})`
保存有效期十分钟的预览。操作 id 为 1–128 个 ASCII 字母、数字、`_` 或 `-`。
`kind` 为 `correct`、`suppress` 或 `delete`，回执包含精确的带版本目标列表、排除来源、
范围和 `preview_digest`。

`productCommit(auth, operation_id, preview_digest)` 重查记录、来源闭包和当前权限。
相同操作/输入的重试恢复同一结果，改变输入会冲突。
`productChange(auth, operation_id)` 查询状态；`productDiscard(auth, operation_id)`
废弃未提交预览并清除其内容副本。过期或废弃预览不能提交。

更正要求记录是 active 的 support Assertion，且 actor key 等于已验证调用者的 principal id。
仅接受当前 Schema 支持的 Concept 值记录；`new_value` 非空白且最多 8192 UTF-8 字节。
一个原生 `MUTATE` 撤回旧主张，创建新的用户 Evidence、带类型的值、归属明确的 Assertion
与来源 Activity；不会覆盖 Assertion，也不会把其他 actor 的证言当作调用者的新陈述。

抑制归档、删除清除已审阅闭包：Proposition、Assertion、引用的 Evidence 和记录的反向依赖，
最多 128 个元素。共享 Evidence 会扩大范围，其来源身份会出现在预览。包含 Concept 的闭包、
未知来源或 legal hold 会被拒绝。语义端点 Concept 不会自动清除。独立记录、外部聊天、文件、
日志、备份及已交付上下文不属于该范围。

首次原生修改前，宿主持久化来源封禁、新处理代次和待完成操作。此后所有代理读取、词汇发布、
写入与最终返回都检查该代次；旧 Formation、Maintenance、Recall 必须重建上下文，HTTP
返回 409 而不是交付过期内容。Worker 没有持久化模型历史或 Notes 缓存；修改完成后会清理
持久维护 assessment 中的旧内容副本。

每一步原生操作有稳定幂等键和已完成进度。`committing` 或 `reconciling` 期间自动处理不可用。
后续对象访问、驱逐后重新加载都会按当前原生权限重试已准入工作；权限、hold 或版本冲突仍
无法解决时保留封锁。不要清除封锁或重放整份计划来掩盖未决写入。只有原生回读核验与相关
预览副本清理完成才报告 `confirmed`。回执保留 id/摘要，已删除内容被移除；仍存活的独立
更正保留其自身来源文本。

闭包按带版本的逐项操作处理，留下身份 stub，并非跨操作原子批。这样不会让引擎 cascade
悄悄删除预览以外的目标。管理修改后，模型历史/非活跃选择器、所有 continuation cursor
以及 SEARCH 之外的 META 均被禁用。管理级 `execute_kip_readonly` 仍可审计历史，不应作为
模型读通道；管理级原始写入也不属于该受控产品通道。

## 接收者拥有的记录 Watch

将 `BRAIN_PRODUCT_RECIPIENT` 设置为显式注册的原生 principal id。宿主仍需提供经过验证的
身份与原生权限；配置项不会授予权限，也不会从旧 HTTP API Key 推导身份。

- `productCreateRecordWatch(auth, operation_id, assertionId, summary)`：先持久登记工作，
  再创建并 arm 结构化 delta Watch。summary 非空，最多 4096 UTF-8 字节。重试恢复同一
  Watch，不重新 arm 旧 generation；创建在 `preparing` 中断时需重试同一 create 调用。
- `productRecordWatch(auth, operation_id)`：读取该接收者的原生状态。
- `productAdvanceRecordWatch(auth, operation_id)`：读取当前整体版本和 WatchState generation，
  交给原生引擎推进至多 200 条变更；是否触发由原生授权覆盖决定。
- `productCancelRecordWatch(auth, operation_id)`：归档 Watch，保留 generation 和 checkpoint。
  重试创建或取消都不会重新 arm。

这是由宿主轮询的订阅，没有后台 inbox 投递、任意回调、语义条件、业务执行或自动权限授予。

`productStatus()` 为受信宿主返回代次、可用性、待完成 key 与学习就绪诊断。
学习保持 `services_missing`、`supported:false`，需使用具有显式 executor/observer/source
和校准绑定的 Rust 学习运行时。模型调用成功或 Watch 触发不代表独立 Outcome、真实收益或
执行某个程序的权限。
