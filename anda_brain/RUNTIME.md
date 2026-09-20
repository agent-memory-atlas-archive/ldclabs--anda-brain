# Runtime integration

**[English](RUNTIME.md) | [中文](RUNTIME_cn.md)**

The Rust service schedules persistent attention and runs explicitly configured
memory consumers. Structured Watch scheduling is enabled for registered Spaces;
action delivery, semantic evaluation, learning, utility and trust each require
their own bindings and switches. No model text or ordinary request installs an
executor, observer, evaluator or governance authority.

| Reference | Purpose |
| --- | --- |
| [API](API.md#authenticated-runtime-inbox-and-observations) | HTTP/MCP authentication, payloads and status |
| [Learning](LEARNING_RUNTIME.md) | Paired trials, business HTTP adapter, archive and review |
| [Utility](UTILITY_RUNTIME.md) | Delivery receipts, contribution and calibrated ranking |
| [Semantic Watch](SEMANTIC_WATCH_RUNTIME.md) | Model contract, coverage, budgets and recovery |
| [Contextual trust](TRUST_RUNTIME.md) | Independent verification, scoped proposals and governance |

## Startup

`BRAIN_RUNTIME_CONFIG=/absolute/path/runtime.json` or `--runtime-config <path>`
loads a versioned JSON file in the existing HTTP and MCP stdio startup paths.
Space names follow AndaDB's 1–64 character lowercase/digit/underscore rule.
It must be installed before sharing AppState or loading Spaces. The built-in
adapter ID is `attention_inbox_v1`; unknown IDs, invalid budgets, missing secret
references and an unmapped recipient fail configuration. This adapter writes an
actual durable, idempotent inbox delivery using the native act/Attempt/fence path.
It does not infer that a human read the message, or automatically create an Outcome.

An example configuration is in [runtime.example.json](runtime.example.json).
Replace the signed CWT subjects and observer contract digest with the actual
deployment's identities and method configuration. Subject strings name claims
verified with `ED25519_PUBKEYS`; they are not semantic actors. Space-token mappings
use `space_token_env` to resolve an already minted token from a named environment
variable. Only its digest is retained in the mapping. No credential value, tool URL
or shell command enters an action/observation request.

`bootstrap:true` explicitly provisions native Principals and the required grants
for the named controller/readers/observers. Controller and observer remain
separate. Durable markers prevent a restart from restoring revoked grants; an
interrupted, unresolved provisioning step requires operator review. With
`bootstrap:false`, the trusted host provisions native governance itself.
Changing authentication/authority can invalidate existing Watch bases: inspect old
work before explicitly migrating pins/re-arming. No automatic re-arm is performed.

The adapter can use an explicit `ContextRequest` or find a real, current
Proposition coordinate for a simple notification. A coordinate is not a truth
claim. The action gate reads constraints/BELIEF and applies its packet budgets. With no
real coordinate the gate remains blocked. An optional configured question uses
the separate clarification flow; a recorded answer suppresses an unsent question.

Embedded hosts can install `MemoryRuntimeBindings` via
`AppState::with_memory_runtime_bindings`, reusing their `ActionBindings`.
Do not install both global and per-Space action adapters. `Space::memory_runtime()`
exposes the trusted facade and `consequences()` its authenticated Rust ingress.
Run the existing background lifecycle and await shutdown. Isolated forks do not
inherit live runtime bindings; shutdown drains admitted observation writes before
closing learning, attention and AndaDB.

## Authentication

- Inbox/status require a real `read`/`*` CWT or verified Space token, plus an
  explicit mapping and audience. Public Space visibility and local auth-disabled
  fallback do not authenticate a runtime caller.
- Responses require `write`/`*`, current native visibility and the actual
  clarification recipient. Answers are data, never consent or execution authority.
- HTTP outcomes require a **signed, explicitly observer-mapped CWT** with
  `write`/`*`, a matching observer contract and current native `record_outcome`
  authority. Ordinary write tokens and Space tokens cannot become observers.
- Without `ED25519_PUBKEYS`, independent HTTP observation intake remains disabled;
  explicitly mapped Space tokens can still read/respond. Direct Rust hosts may
  supply genuine native authentication contexts.

## Host integration

The HTTP service and MCP stdio server already call `AppState::start_background_tasks`.
Embedded hosts must run that lifecycle with a cancellation token. Configure
`AppState::with_attention_policy(AttentionPolicy)` before cloning the host or opening
a Space. Defaults enable scheduling; isolated experiment/shadow hosts remain off.

| Trusted Rust API | Purpose |
| --- | --- |
| `AppState::attention_tick()` | One bounded pass, also used by the service timer |
| `Space::attention()` | Access this Space's trusted runtime |
| `AttentionRuntime::arm_watch(id, version)` | Register/dirty first, then native arm |
| `register_work()` | Must precede direct Nexus work creation |
| `status()` | Registration, last scan, bounded report and retained rechecks |
| `runtime_status()` / model `memory_runtime/status` | Read actual attention/action configuration |
| `semantic()` / `semantic_status()` | Optional semantic evaluator and its separate status |
| `configuration()` / `reconfigure_evaluator(version)` | Explicit CAS migration; never auto-rearm old Watches |
| `actions()` | Access the optional action runtime |
| `set_enabled(bool)` | Explicit per-Space scheduling switch; isolated hosts cannot enable automation |
| `schedule_recheck(Recheck)` | Record defer/review/dependency revalidation |
| `observe_basis(&ProjectionBasis)` | Retain native `next_invalid_at` for an already registered Space |
| `acknowledge_recheck(key, due_at_ms)` | Remove the exact obligation after a trusted consumer handles it |
| `register_resume_verifier(condition, pin, verifier)` | Install bounded host code for OnChange; reinstall after restart |

The model's existing `memory_runtime/arm_watch` calls this same boundary. Native
attention already present in a Space is registered on its first load. Spaces never
registered with Brain are not found by scanning the resident map; existing direct
Nexus hosts must perform the registration step before claiming durable scheduling.
Learning mutations installed through a Space also register first. Review hints are
discovered for enabled registrations. The learning runtime starts independently owned learning passes
only with actual bindings and explicit automation switches; business I/O does not
hold the Watch scan. See [LEARNING_RUNTIME.md](LEARNING_RUNTIME.md).

Present-time, directly returned native BELIEF bases can add a small asynchronous
directory hint. Raw cognitive JSON, historical/what-if queries and model-provided
timestamps do not enter that path. It performs no cognitive or native-control
writes and adds no wait to the read-only KIP/model tool's timeout. Owned hint writes
drain on close; registered Spaces are periodically reconciled if a hint is missed.

## Action callbacks and authority

Install `AppState::with_action_bindings(ActionBindings)` before sharing the host
or loading a Space. The value contains Rust trait objects, never deserialized model
parameters. Its effective policy digest covers the supplied policy pin, adapter
pin, channel/observer configuration and budgets. Changing these does not silently
adopt work armed under another configuration.

| Interface | Host responsibility |
| --- | --- |
| `ActionIdentity::authenticate(scope)` | Establish fresh direct Nexus authentication for each operation |
| `ActionPolicy::context(wake)` | Declare bounded, task-specific required reads and prerequisites |
| `ActionPolicy::suggest(input)` | Suggest act/ask/defer/silence from the counted packet |
| `ActionPolicy::authorize(request)` | Check current business policy, task scope and allowed content |
| `ActionPolicy::allow_silence(input, reason)` | Explicitly permit a quiet-policy/resolution decision; defaults false |
| `ActionExecutor` | Fix target/credentials; verify environment, tools, budget and current target permissions; enforce lease deadline at the effect |
| `ActionLookup` | Query the actual target by the same attempt and return freshly authenticated observation |

An installed host must provision the Nexus Principal and appropriate grants before
arming. `space.attention().actions().unwrap().nexus()` gives trusted Rust code the
native governance APIs for this provisioning. This Rust builder grants nothing
automatically; the startup file can separately opt into `bootstrap` as described above.
The workflow uses `read`, `read_history`, `read_governance_history`, `project`,
`create`, `update`, `derive` and `maintain`, subject to current resource restrictions.
Lookup uses a separately configured, directly authenticated `record_outcome`
Principal. Neither a semantic actor nor a normal Brain write token supplies these
host capabilities. Each executor still enforces its own target-system authority.

## Reads and decisions

`ContextRequest.anchor` names an existing Proposition solely to obtain a native
BELIEF coordinate. It need not be true. `premises` separately names propositions
that must currently be `accepted`. Native projections retain opposition, conflicts,
temporal validity and uncertainty. Required references, exact SkillRevisions, the
Watch/fire and all currently visible Commitments are read at a fixed snapshot.
Task-specific constraints outside Commitments belong in `required_refs`.

The existing `recall_budget::pack` retains required items as one indivisible set.
A full/truncated constraint page, masked/missing required item, invalid dependency,
oversized packet/input or unverified premise cannot authorize act. The packet
continues to report `semantic_complete=false` and `action_ready=false`. Suggestions
can only name delivered `used_refs`; retrieved, used and applied revisions remain
distinct. Applied revisions must be used, match the task family and pass native
current-revision, executable-authority and dependency checks before dispatch.

| Decision | Durable result |
| --- | --- |
| act | Decision + non-trial Attempt + fixed-request artifact + dispatch continuation |
| ask | Ask Decision + fixed recipient/question/deadline + clarification and answer continuations |
| defer | Decision with reason + timed retry or manual recovery continuation |
| silence | Decision with explicit host-policy or verified deduplication basis; no dispatch |

Each gate's Decision/Attempt outputs, continuation wakes and its own completion
commit atomically. The immutable request artifact and serialized CAS journal
preparation are acknowledged first; an interruption can leave an unused artifact.
Native receipt replay repairs a missing journal acknowledgement without creating
another logical decision or Attempt. A configured business deduplication key is
scoped to the Space instance and bound to the request/configuration; an unresolved
owner defers duplicates instead of creating another action.

## Clarification

The parent remains `ask` and has no Attempt. A separate, deterministic clarification
worker reads the committed parent Decision, then creates a child `act` Decision
and Attempt with the parent as a real, version-pinned input. It never invokes the
suggestion model or recursively asks. Its dispatch uses `DeliverClarification` and
the configured clarification executor; it grants no business permission.

`ActionRuntime::respond(gate_wake_ref, fresh_auth, ClarificationResponse)` accepts
only the configured recipient before the deadline. Identical responses replay;
changed bytes conflict. An answer creates no permission: the existing answer
continuation runs a fresh business gate and rechecks current policy. Timeout creates
a defer receipt and manual work, never implicit consent. An expired question is not
sent. External send ACKs do not complete the observation obligation.

## Dispatch and recovery

Immediately before external I/O, Brain checks the immutable native request artifact,
current policy/target authorization, fresh BELIEF/constraints, exact context pins,
then calls native `begin_wake_dispatch` under a live owner/fence. The external key
is always the committed Attempt identity. Caller cancellation detaches from an
owned task; it cannot interrupt a native commit. Callback timeout becomes unknown
delivery, never a manufactured failure or success Outcome.

Non-idempotent uncertainty requires lookup; no lookup leaves explicit blocked work.
Only an authoritative, freshly authenticated native `NotStarted` reconciliation or
a verified target idempotency contract permits another send with the same key.
The observer/configuration is fixed before first dispatch. Unknown never authorizes
resend. `Accepted`/`Finished` are delivery states, not Outcomes. Native dispatch
wakes remain open until independent terminal Outcome reconciliation; the runtime API supplies
the general authenticated ingestion path. Local atomicity does not claim exactly-once
external effects.

| Control | Meaning |
| --- | --- |
| `AttentionRuntime::runtime_status()` / model `memory_runtime {operation:"status"}` | Read configuration and bounded scan report; `ready:null` requires per-work validation |
| `AttentionRuntime::actions()` | Optional per-Space trusted runtime |
| `ActionRuntime::status(wake_ref)` | Retained gate/Attempt/dispatch refs, state, reason, retry position |
| `ActionRuntime::set_enabled(false)` | Persistently pause new action work while continuing Watch discovery |
| `ActionRuntime::retry(wake_ref)` | Explicit operator retry window, same attempt and all current checks |
| `AttentionRuntime::set_enabled(false)` | Pause all scheduled attention work for this Space |

With no bindings, wake discovery reports `action_bindings_not_installed` and retains
pending work. A changed policy/binding or Watch generation requires explicit host
migration/re-arm review; old pending/unknown external work must be reconciled before
replacing it. Runtime artifacts use the reserved directory's `actions/` prefix and
conditional updates. One live owner per shard remains required. Isolated forks do
not inherit executable callbacks. Close drains action writes before closing AndaDB.

## Action budgets

Installed bindings default to 2 work items per Space/pass, a 4,096-token Recall
packet, 32,768-token serialized gate input, 2,048-token suggestion, 5-second callback
and total read-phase deadlines, a 60-second real lease and 60-second initial retry intervals.
There are at most 3 automatic gate retries; send/lookup share a bounded retry window.
Exhaustion retains operator work. Model adapters must also enforce their provider's
request/output budgets. Native/storage commits are drained rather than timed out.

Mechanism tests live in [space/tests/action.rs](src/space/tests/action.rs), using
real Nexus/AndaDB transactions and deterministic local adapters. They test all four
branches, required-context refusal, current authority/revision checks, clarification,
CAS/ACK loss, native PUT suspension, cold recovery, lookup and fair scheduling.
They make no network/model calls and do not establish model quality, learning gains
or a production adapter's correctness.

## Storage and recovery

The reserved prefix is `__brain_runtime__/v1/shards/{shard}/`. It contains a CAS
root, stable numeric index slots, versioned registrations, partial scan checkpoints
and bounded retry records. Slots avoid relying on ObjectStore listing order. An
interrupted allocation can leave an empty slot; no native work can arm until its
registration and index have both been read back. A registration binds the external
Brain Space ID to the immutable native instance and pins, with an explicit mapping
to the retained Nexus attention scope. Native KIP operations use `DEFAULT_SPACE`;
its name is not confused with the external Brain Space ID. Scope, shard, format
or instance mismatches are rejected.

Directory and Nexus commits are not one transaction. Registration/dirty precedes
work; a scan acknowledges only the exact version and dirty generation it covered.
Missing ACKs are checked against retained bytes/receipts. A partial pass keeps its
Watch cursor and any unprocessed wake IDs; stale scans cannot clear newer work.
Every registered slot is revisited even when its cached due time is far in the
future. Structured and diagnostic Watch scans have separate cursors. Unsupported
text/legacy work and failed resume checks retain explicit reasons and backoff.

Storage must support conditional updates. The CLI's local backend already uses
`MetaStoreBuilder<LocalFileSystem>`; raw `LocalFileSystem` alone does not provide
the required CAS. InMemory and the configured S3 backend also support it. Keep
**one live Brain owner process per storage shard**. This is not cross-host storage
ownership/fencing or a multi-writer failover protocol.

## Budgets and lifecycle

| Default | Value |
| --- | --- |
| Tick | 5 s |
| Registered slots per tick | 20 |
| Runnable Watch page per Space | 20 |
| Diagnostic Watch page per Space | 20 |
| Change envelopes per Watch | 200 |
| Wake history scan per Space | 200 |
| Admission wall budget | 10 s |
| Reconciliation and blocked retry | 60 s |
| Without action bindings: model calls/tokens/external sends | 0 |
| Runtime admission queue | 16 |
| Retained rechecks | 128 |
| Directory slots | 1,000,000 |

The wall budget stops admission of more work. Already admitted native/storage
writes finish; a timeout is not allowed to tear an atomic commit. Async resume
verifiers have a separate timeout outside the native write lock. Shutdown closes
admission and drains owned tasks before DB recovery/close. `Space::is_busy()` also
accounts for observed live attention leases. Formation/Maintenance retain their own
processing gates. Attention-only loads do not start Formation/wiki recovery or
refresh user access time; a later normal load still resumes those queues.
Installed action callbacks have their own bounded admission and may run on cold load.

Engineering tests use local/in-memory storage and deterministic callbacks. The
20-Space/200-Watch test checks the P95 ≤ 60 s threshold with a 5 s tick;
it is not a measured latency promise for a particular S3/network deployment.
After downtime the service catches up; it does not promise on-time execution while stopped.

## Observation recovery

The off-graph CAS journal records the exact authenticated input and native intent
before mutation. Caller cancellation cannot tear admitted writes. Lost native or
journal ACKs replay the same event/intent under fresh authentication. Native
Evidence commit and dispatch reconciliation are separate steps; their durable refs
allow retries without another Outcome. The audit index publishes immutable receipt
snapshots only after storage acknowledgement; consumers deduplicate by receipt ID
and native ref. `ConsequenceRuntime::receipts(auth, lane, after, limit)` provides
bounded, currently authorized audit discovery, including pending safety signals.

Inputs are capped at 16 KiB, event keys at 256 bytes, runtime mutation admission at
16 concurrent jobs, and each observation index at one million events. Detailed
measurement bodies, late/correction handling and learning routing are in the
[API](API.md#authenticated-runtime-inbox-and-observations).

## Release scope and validation

KIP v2 has not been deployed. This release starts with fresh v2 Spaces; migration
inventory and orphaned pre-release Watch recovery are outside the current scope.
Register every Space before relying on background scheduling. Normal restart,
eviction, acknowledgement-loss recovery and configuration changes remain supported.

Status reports contain bounded scan/work counts, reasons and retained references.
They are not a complete operational inventory or an aggregated latency, backlog-age
or cost dashboard. Measure production storage/provider latency separately. Mechanism
tests establish recovery and authorization behavior; actual learning improvement,
semantic miss rates, utility attribution and trust calibration require independent
business-data validation in MIB before enabling the corresponding automation.

See the [observability and empirical validation plan](../VALIDATION_PLAN.md) for implementation steps and acceptance gates.
