# Learning runtime

**[English](LEARNING_RUNTIME.md) | [中文](LEARNING_RUNTIME_cn.md)**

The learning runtime connects the existing native paired controller to independently scheduled
host work. It adds a compiled `workflow_http_v1` business adapter, permanent
history indexes, terminal archival and a consumer for authenticated safety
obligations. Supported family: **`tool_workflow.precondition.v1` only**.
Compilation and successful startup do not establish empirical learning improvement.

## Enablement

Build the full service with `--features mcp,wiki,learning`, then load trusted
`BRAIN_RUNTIME_CONFIG` / `--runtime-config`. Each Space may add a `learning`
object alongside the optional inbox `adapter`. See
[learning.runtime.example.json](learning.runtime.example.json).
The example is a template: automation and calibration approval are off;
replace its pins/material/endpoints with reviewed deployment data before use.
Its numeric thresholds are illustrative, not production calibration defaults.

The adapter resolves three distinct bearer credentials from environment
references: executor, independent observer and registered plan source. URLs
come only from startup configuration; HTTPS is required except for loopback
HTTP. Redirects are disabled. Credentials never enter plans, prompts or receipts.
Normal HTTP/MCP callers cannot register executors or enroll cohorts.

Startup checks the actual executor capability response and freshly authenticates
the observer, including current Nexus `record_outcome` authority. Each automatic
trial pass repeats these checks before new execution. Explicit `bootstrap`
can provision the observer through the revocation-preserving provisioning path.
It never grants executable authority to a SkillRevision; that remains a reviewed
native governance operation.

`LearningCalibration` retains distinct training/validation manifests, the actual
report, reviewer identity, environment and the digest returned by
`calibration_contract(&registration)`. Automatic trials/reviews require explicit
approval of those exact inputs. Changing model, tools, budget, observer semantics
or calibrated bounds changes that contract. This validates an operator's reviewed
configuration; it does not independently prove the report's empirical quality.
Test approvals and deterministic model endpoints are mechanism fixtures only.

Trusted Rust hosts can instead install `LearningBindings` in
`MemoryRuntimeBindings.spaces[space].learning`, or call
`LearningRuntime::install_bindings(AuthContext::system(), bindings)` before
automatic work. Callbacks implement `LearningExecutor`, `LearningObserver` and
`LearningPlanFactory`. `preflight` must verify reset, lookup, budgets and cancellation;
the default executor probe refuses automatic installation. Registration remains
immutable for the Space's native instance. Reopening the Space reinstalls the
same code binding; copying its operational identity remains forbidden.

## Scheduling and status

The attention tick starts at most one owned learning pass per Space. It does not wait for
business I/O before continuing the Watch scan. Each pass admits one hot job step,
one independent observation, an eight-slot archive review page, at most one
authorized review enrollment and an eight-slot safety receipt page. Hot job and
review cursors persist. New work requires a registered factory's frozen manifest;
a Watch firing alone has no enrollment authority. New dispatches retain origin
in the journal/ticket and a `host_enrollment` JSON string inside native
`DecisionRecord.rationale`.
Legacy records can have no origin. A supplied gate reference must resolve to its
actual completed wake and firing activity.

The four independent switches are `automation.trials`, `reviews`, `archive` and
`safety`. Host `automatic=false` prevents all scheduled work, including isolated
experiment hosts. Disabled registration prevents new comparisons/trials; explicitly
enabled safety recovery and archival can still resolve existing obligations.
Callbacks are bounded to 30 seconds at most, and each actual dispatch uses the
shorter remaining native lease/attempt budget. Shutdown cancels callbacks and
drains owned persistence. An uncertain send always keeps its original identity.

`GET /v1/{space_id}/runtime/status` and `anda_brain_get_runtime_status` add `learning`:
`compiled`, `registered`, `registration_enabled`, `bindings_ready`,
`automatic_allowed`, `running`, `automation` and `blocked_reasons`.
Only explicitly mapped recipient auditors receive `capacity` and `last_pass`;
other callers receive null for those inventories. Counts describe work/checkpoints,
not positive verdicts. Maintenance does no inline learning execution: its original
Skill counters stay zero, `skills.runtime` reports the separate scheduler and
`unsupported_reason` explicitly explains this separation.

## Business host protocol

The three configured base URLs end in `/`. All responses are JSON and capped at
256 KiB. Non-success HTTP responses, invalid identities, truncated logs, timeouts
and unknown transport results never imply success or authoritative `NotStarted`.
Rust request/response types are in [workflow_http.rs](src/learning/workflow_http.rs).
The HTTP adapter is production code; the loopback test server is test code.

| Channel | Method | Contract |
| --- | --- | --- |
| executor | `GET capabilities` | `WorkflowCapabilities`: exact identity, calibration pin, isolated reset, idempotency, authoritative status, fence/deadline enforcement, cancellation and complete instrumentation |
| executor | `POST reset` | Frozen dispatch header → `WorkflowReset`; reproduce exact task and initial state digests in an isolated world/session |
| executor | `POST decide` | Public task, frozen treatment procedure or null, actual replies and remaining token budget → `WorkflowChoice`; use pinned model and factual memory |
| executor | `POST inspect`, `prepare`, `commit` | Dispatch header + request digest + sequence + stable idempotency key → `WorkflowReply`; enforce pins, fence, deadline and tool budget at the target |
| executor | `POST finish` | Seal complete actual journal; acknowledgement alone does not create an Outcome |
| executor | `POST cancel` | Idempotently stop outstanding work; enforce lease expiry even if the client disappears |
| executor | `POST status` | Same dispatch header → `WorkflowStatus`, with matching request digest |
| observer | `GET identity` | Separately authenticated exact `ObserverControl` |
| observer | `POST journal` | Same dispatch header → `WorkflowJournal` or null; read actual durable ordered state transitions and all model usage, including failed/retried operations |
| source | `POST enrollment` | Instance, source pin, last accepted event or review obligation → frozen `LearningEnrollment` or null |

The dispatch header contains `format`, dispatch/attempt/task identifiers, native
fence and deadline, frozen `PairCase`, environment/model/base-memory/tool pins and
the full budget. It is a trusted host envelope. The business model receives only
the task, tool replies, frozen procedure and its context/budget pins. Do not pass
case seeds, baseline/treatment labels, hidden requirements, evaluation results or
validation journals to the model.

The observer verifies sequence continuity and actual before/after states, counts
every failed commit and forbidden preparation, and uses measured elapsed/token
usage. Missing usage stays null/unknown. The exact raw journal, at most 16 KiB,
is retained in protected host storage before submitting the typed outcome API.
`LearningRuntime::observation_replay(job_id, dispatch_id)` resolves it for trusted
audits. It is not exposed by Recall, HTTP or MCP. The outcome service retains the single native
Outcome writer and its fixed-cutoff/idempotent recovery rules.

The host service must implement actual operations, logging and model execution;
the adapter does not supply these systems. Installing the adapter requires those
services to pass probes. A successful disk-backed loopback test proves the wire,
reset and persistence mechanisms, not a deployed business system or model benefit.

## Archival, review and safety

`LearningConfig.maximum_jobs` is now **hot working-set capacity**, still 1–32.
`jobs()` returns only that set. `jobs_page(after, limit)` walks retained identities
by numeric slots, with `limit` 1–32 and explicit `next_after`/`complete`.
`report(job_id)` resolves either tier. `reviews_page` uses a separate persistent
review index plus the bounded hot set. Legacy `reviews()` refuses to claim a
complete inventory when it needs another page.

Before archival the runtime resolves pending intents and reads terminal native
receipts, all Attempt/Outcome/Trial/Evaluation and acquisition references. Unknown
external effects, missing Outcomes or unresolved safety records retain their hot
slot and report backpressure. A completed native dispatch can acquire a new lease
solely to finish expired task bookkeeping; it cannot execute or resend business
work. Missing/unknown outcomes cannot use that path.

Archival removes the identity from the hot directory and creates a verified,
immutable terminal snapshot. Canonical jobs remain addressable for later review
links and safety checkpoints. `archive_replay(job_id)` returns the original
checkpoint; current `report` may contain a later revocation. Native evidence is
never deleted, and job IDs cannot be reused with different inputs. Indexes retain
Attempt routing, evaluation lookup and review obligations across eviction/restart.

`LearningStoragePolicy` separately bounds retained identities and logical host
journal reservations: default 4096 records and 64 MiB reserved per identity;
operators can set lower limits. Each JSON journal object is capped at 8 MiB.
This reservation covers the live/enrollment/archive journals and bounded replay
allowances; it is not measured physical usage or a retention policy for Nexus
evidence. Capacity exhaustion never consumes a review obligation. No automatic
evidence deletion, compaction or expired-unknown abandonment is performed.

The one-time v1 migration imports its bounded (at most 32) directory, retains old
IDs/instance/configuration/replay records and publishes the new catalog recoverably.
After migration discovery uses numeric slots, not object-store listing order.
Archive and enrollment writes use CAS and exact readback; incomplete publications
remain recoverable. Use one active owner per shard and conditional-write storage.

When no new monitoring cohort is authorized, the review remains due and current
recommendation eligibility expires. The review timer is acknowledged only after
a durable child owns the obligation; failed/expired unenacted children make the
original obligation discoverable again. Late/conflicting authenticated safety
receipts remain eligible for the separate safety consumer. It checkpoints only
after native revocation, with fresh observer authentication; it does not amend an
old trial score. `safety_pending` means the obligation is still unresolved.
Resolved receipts retain `safety_evaluation_ref`. Another signal may be covered by
that exact current revocation; after re-entry, routing resolves the current
revision's controller job instead of rewriting an old signal/verdict.

## Validation boundary

Mechanism tests cover 34 sequential native terminal jobs with archival, cold
replay, archived adoption/review/revocation, missing-cohort and capacity behavior,
v1 migration without later listings, archive ACK/checkpoint failures, real HTTP
startup and disk-backed reset/journal operations, and native lease expiry recovery.
Existing fixed-cutoff, unknown-baseline, missing-treatment and cancellation tests
remain part of the full matrix.

MIB must separately pin train/validation splits, environment, model, token/tool
budgets, baseline and missingness, then demonstrate the predeclared effect lower
bound and failure-rate ceiling including later drift/revocation. This repository
adds no replacement offline evaluator and makes no empirical improvement claim.
