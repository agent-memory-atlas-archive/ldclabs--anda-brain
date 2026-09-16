# Anda Brain — Technical Documentation

A dedicated LLM-powered memory management service that maintains a persistent **Cognitive Nexus** on behalf of business AI agents via [KIP 2.0 (Knowledge Interaction Protocol)](https://github.com/ldclabs/KIP).

Business agents interact entirely through natural language and a REST API — no KIP knowledge required.

Anda Brain is designed to be **self-hosted** (the hosted cloud service has been discontinued). For a complete agent built on Anda Brain, see [Anda Bot](https://github.com/ldclabs/anda-bot).

SleepTask and Watch are exempt from bulk mnemonic decay because all operational
record updates require version guards. Failed host passes reach the model in
assessment.settlement_errors.

## KIP 2.0 / CognitiveMemory 2.1 update

Rust uses published `anda_kip`, Cognitive Nexus and AndaDB 0.13 packages;
the Worker uses published `@ldclabs/kip-do` 0.13. Skill behavior is an
immutable `SkillRevision`; Watch progress and task leases use protected Nexus
operations. The former family-rate Skill promotion rule has been removed. Without
configured independent observers, frozen trials and replayable evaluations,
procedures remain unproven and `skills.unsupported_reason` reports the limitation.
Existing Brain endpoints remain available. The optional five-intent Memory Interface
and its `memory_*` bundles are **not advertised** by these adapters.

## Architecture

```
┌─────────────────────┐
│   Business Agent    │  ← Focuses on business logic & user interaction
│  (No KIP knowledge) │    Only speaks natural language
└────────┬────────────┘
         │ Natural Language / REST API
         ▼
┌─────────────────────┐
│      Brain          │  ← The ONLY layer that understands KIP
│   (LLM + KIP)       │    Three agents: Formation / Recall / Maintenance
└────────┬────────────┘
         │ KIP (KQL / KML / META)
         ▼
┌─────────────────────┐
│  Cognitive Nexus    │  ← Persistent Knowledge Graph (backed by AndaDB)
│  (Knowledge Graph)  │
└─────────────────────┘
```

## Features

- **Zero KIP knowledge required** — Business agents interact through natural language and a simple REST API.
- **Persistent, structured memory** — Facts, preferences, relationships, events, and patterns encoded into a knowledge graph.
- **Three operational modes** — Formation (encoding), Recall (retrieval), and Maintenance (consolidation & pruning).
- **Multi-space isolation** — Each space has its own independent database, knowledge graph, and conversation history.
- **Triple serialization** — Supports JSON, CBOR, and Markdown for request/response payloads (negotiated via `Content-Type` / `Accept` headers).
- **Built-in MCP server** — MCP-capable agents can use Anda Brain through Streamable HTTP or stdio tools without writing REST glue code.
- **Pluggable storage backends** — Local filesystem, AWS S3, or in-memory (for development/testing).
- **MIB offline regression** — Product timelines and business outcomes are evaluated by the independent MIB runner; Brain retains online diagnostics and native learning-mechanism tests.

## Agents

### Formation — Memory Encoding (`formation_memory`)

Receives conversation messages and encodes them into structured memory within the Cognitive Nexus via KIP.

**System prompt:** [BrainFormation.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainFormation.md)

**Processing pipeline:**
1. Receives `FormationInput` (messages + optional context + timestamp).
2. Creates a tracked `Conversation` record (status: `Submitted` → `Working` → `Completed` | `Failed`).
3. LLM classifies what the conversation is worth keeping, into the products the
   Cognitive Memory Profile defines: `Evidence` for what was observed,
   `Proposition` + `Assertion` for a truth-sensitive claim and whose stance it
   is, `Event` for what happened, `Experience` + steps when the process itself
   can teach future behavior, `Commitment` for a future obligation, and
   `Insight` / `SelfModel` candidates. The empty write is a valid answer.
4. Grounds against existing memory before writing (SEARCH before CREATE).
5. Encodes it through the `execute_kip` tool, which on this path accepts KQL and
   META in full and only the cognition subset of KML — administering memory in
   bulk (`UPDATE`, `SET RETENTION`, `PURGE`, `MERGE CONCEPT`, and `TRANSITION`
   to `archived` or `tombstoned`) is refused to a pass whose whole input is an
   untrusted conversation.

**Key behaviors:**
- Sequential processing with automatic queue draining — new conversations are picked up after the current one completes.
- Atomic single-conversation processing via `processing_conversation` flag.
- New vocabulary enters through the host, never through KML. KIP 2.0 makes
  Schema protected control state, so a command naming an undeclared type or
  predicate is refused with `SchemaSymbolNotFound`; the model asks for one
  through the `declare_memory_symbols` tool, and the host validates the name's
  shape, caps how many a space may hold, and versions the result.

### Recall — Memory Retrieval (`recall_memory`)

The optional `RecallInput.budget` (or an enforced `memory_policy.recall_budget`)
selects a host-packed JSON memory response instead of free-form synthesis.
The whole packet and cumulative normalized planning input use the pinned
`o200k_base@tiktoken-rs-0.12.0` counter. Required commitments/warnings precede
optional items, failed coverage is explicit, and diagnostic histories/artifacts
cannot bypass the packet limit. Existing requests without a budget policy keep
the normal flow below. See [P5 contract](API.md#recall-budget-contract).

Translates natural language queries into knowledge graph lookups and returns synthesized answers.

**System prompt:** [BrainRecall.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainRecall.md)

**Processing pipeline:**
1. Receives `RecallInput` (query + optional context).
2. Analyzes query intent (entity lookup, relationship traversal, attribute query, event recall, pattern detection, etc.).
3. Grounds entities to actual graph nodes (resolves ambiguity).
4. Executes structured KQL/META reads; belief questions go through `BELIEF`
   projection rather than raw `FIND`, because a Proposition existing is not the
   Proposition being true.
5. Iterative deepening — follows up with additional queries if needed, up to
   the space's `recall_max_rounds` (default 7).
6. Synthesizes results into a coherent natural language answer, reporting
   contested as contested and insufficient as insufficient.

**Available tools:**
- `execute_kip_readonly` — KQL and META only, enforced on what each command
  parses to. Recall has no write tool at all, and reading never reinforces what
  it read.
- `wiki_search` / `wiki_read` — with the `wiki` feature; absent otherwise.

### Maintenance — Memory Metabolism (`maintenance_memory`)

Consolidates, prunes, and optimizes the knowledge graph during scheduled or on-demand cycles.

**System prompt:** [BrainMaintenance.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/BrainMaintenance.md)

**Processing phases (full scope):**
1. **Assessment** — Audit memory health (read-only): `DESCRIBE PRIMER`, pending SleepTasks, unconsolidated Events and Experiences, orphans, stale events, plus the runtime's own `assessment` block (per-predicate census, correction tallies, armed and fired Watches, the current `space_seq`).
2. **SleepTask Processing** — Handle queued work under the Profile's classes: `consolidate`, `review_conflict`, `review_skill`, `resolve_identity`, `review_retention`, `review_derived`, `refresh_self_model`, `inspect_quarantine`.
3. **Semantic consolidation** — Compress clusters of Events, Experiences and Evidence into derived Assertions, keeping Activity lineage back to the sources. A summary is not a new epistemic root.
4. **Procedural consolidation** — Compare successful and failed Experiences and compile an unproven Skill with an immutable SkillRevision. Its task_family identifies comparison candidates; only a configured trial/evaluation pipeline can confer validated standing.
5. **Identity review** — Review `same_as` suspicions, then `MERGE CONCEPT`, which is non-destructive: the source survives as merged historical identity.
6. **Contradiction and derivation review** — Different actors' disagreement coexists; only an actor's own revision supersedes. After a revision, the settlement walks `LIST DEPENDENTS` and hands the agent each revised root with its dependents (`assessment.revised_roots`); the agent flags what no longer holds `stale`.
7. **Mnemonic metabolism** — run by the runtime settlement before the cycle, not by the agent: `MnemonicState.memory_strength * decay_factor` on Concepts due for it. Never `confidence`; a fact nobody has asked about lately is no less credible. `salience` and `utility` stay with the agent — the sweep cannot make a per-memory judgement.
8. **Commitments and Watches** — Review outstanding obligations and attention. Nexus advances structured Watches under generation/CAS/coverage checks; prose conditions remain deferred without a semantic evaluator. No model completion attests change-stream consumption. See the Watch contract below.
9. **SelfModel and WorkingState refresh** — Consolidate identity from evidence rather than from the latest conversation, and rebuild the digest the next waking session resumes from, stamped with the `basis_seq` it was built at.
10. **Retention review** — Decide what should carry an expiry and write it with `SET RETENTION`; the full settlement's sweep is what makes that write mean something. See "Retention expiry" below.

Skill lifecycle transitions require the explicitly configured protected host evaluation pipeline. Standard model-driven maintenance does not grant standing. See "Procedural candidates remain unproven" below.

**Key behaviors:**
- Single-execution guard — only one maintenance cycle can run at a time per space.
- Non-destructive principle — archives before deleting, and weakens mnemonic accessibility rather than removing (never epistemic confidence).
- Async execution — returns immediately with conversation ID; actual processing in background.
- Two triggers, not one. Counting formation conversations paces a space that
  is being written to (daydream every 21, quick every 42, full every 168); a
  24-hour clock covers one that is not. Without the clock a space that stopped
  ingesting stopped metabolizing entirely — no Commitment review, no retention
  expiry, no self-test — which is not what "scheduled, threshold, or
  change-driven" means. The clock fires from the background flush pass for
  resident spaces and on load for spaces that had been evicted; a space that
  has never formed anything is never due.

**Memory policy:** each space carries an evolvable `MemoryPolicy` (stored in
the `memory_policy` extension, set via `update_space`) that holds the numeric
knobs of memory behavior — decay factor and floor, stale-event threshold,
backlog targets, and (from later phases) self-test and recall parameters.
Maintenance cycles without explicit `parameters` run under the space's
policy; an absent policy means the compiled-in defaults, so setting nothing
changes nothing. The policy is the evolution genome of
`docs/memory_evolution_plan_cn.md` (module M-P).

**Mnemonic metabolism:** before each maintenance cycle the runtime runs a
deterministic settlement that decays `MnemonicState.memory_strength` on
Concepts due for it, stamping `last_metabolized_at`. Pinned Concepts are
exempt. Every cycle sweeps; what paces it is the sweep's own weekly
`last_metabolized_at` filter, not the cycle scope, because scope decides how
much *cognitive* work a cycle does and gating metabolism on `full` as well
meant a Space forming slowly went months without any. Newly superseded
Assertions are recorded as corrections and aggregated per asserting actor into
the `source_reliability` extension; full cycles also refresh the per-predicate
census. Both reach the Maintenance prompt as its `assessment` block. What
decays is `MnemonicState.memory_strength` — how *available* a memory should be
— and never an Assertion's confidence: KIP 2.0 forbids letting time erode a
stance, because a fact nobody has asked about in a month is no less credible.
The LLM maintenance agent no longer runs bulk metabolism itself. The last
settlement report is stored in the `memory_settlement` extension.

**Reading does not reinforce.** Every completed recall still records which
graph entities it surfaced, into an off-graph usage ledger (`memory_usage`
collection) that the dream self-test, the health metrics and the scenario
diagnostic inspection reads. That record stops there. An earlier design closed the loop —
settlement raised the recalled Concepts' `memory_strength` by a
`recall_reinforcement` gain — and that is precisely what the reference Recall
policy forbids (§1 "MUST NOT ... change memory_strength, increment recall
counters", §32, invariant 2 "Read does not reinforce memory"). Deferring the
write to maintenance did not make reading stop reinforcing; it only moved
where the reinforcement was written from. So the writeback is gone: a recalled
memory earns no gain and buys no exemption from the next sweep. Retrieval is
observed, not rewarded — which is also the difference between a memory system
and a popularity contest. The `recall_reinforcement` policy knob is retained
for stored-policy compatibility and does nothing.

**Watch progress is protected Nexus state.** Create a Watch as `disarmed`, then
call the internal `memory_runtime` tool with `arm_watch`, its exact id and current
`_system.version`. Arming captures an authorization view and creates a fresh
`WatchState.arm_generation`. Settlement advances structured selectors through a
bounded authorized change page using the current overall version and generation.
Silence requires complete coverage through the deadline; a matching silence Watch
ends as `expired`, counted in the legacy `disarmed` report field. Native advancement
returns status, coverage and a receipt; it does not synthesize `watch_fire` Activities.
Text conditions, including mixed selector/text objects, stay deferred without a
configured semantic evaluator. A completed maintenance model call never advances a
Space-wide consumption watermark. Old Watches without WatchState need explicit
re-arming after their observation gap is reviewed. Firing grants no external authority.

**Procedural candidates remain unproven.** `Skill.current_revision` selects an
immutable `SkillRevision` whose `revision_of` points back to the Skill. Both can be
created atomically. The internal `memory_runtime` tool computes the canonical
SHA-256 digest of all revision attributes except `behavior_digest`; Nexus verifies it.
No model plan can write TrialRecord, EvaluationRecord, AttemptRecord, OutcomeRecord,
TrialState or GradingState. Same-family outcomes are merely comparison candidates;
no automatic baseline or grade is inferred. Settlement preserves the historical
counter fields at zero and includes `skills.unsupported_reason` until a trusted
observer/trial/evaluation scheduler is configured. Historic counters or `adopted`
labels are not validated learning evidence.

### Native learning contracts

With the Rust `learning` feature, `anda_brain::learning`
provides `PairedTrialPlan`, `ExecutionContract`, `PairedRule`, and `register_paired_rule`. A trusted
host freezes the plan as an artifact before baseline execution, uses its pin
for both arms' `AttemptRecord.selection_policy` and `TrialRecord.parameters`,
and registers the rule once per Nexus instance (including after restart).
`attempt_context()` and `comparability()` build the pinned KIP fields.

The `anda-brain:paired-bounded-v2` rule compares one candidate revision against a stable task policy with
the same factual memory, model, tools, budget and task/state/seed pairs. It
requires the entire predeclared cohort and a one-sided Hoeffding lower bound
above the positive practical-improvement margin, and an absolute candidate
failure-rate ceiling. The observer configuration binds the workflow contract
and the complete attempt budget. Missing treatment outcomes
count as failure; missing or unknown control outcomes leave the comparison
insufficient. Duplicate pairs/observations, changed pins, or extra applied Skill
revisions are rejected. A new monitoring trial retains the original acquisition
through `AdoptionBasis`; native tests verify adoption, subsequent revocation and
rejection of re-entry using the old trial. Multi-Skill bundles, adaptive stopping
and filtering a window out of one trial's ledger are not supported. Parameters have no production
defaults and require calibration.

`ExecutionContract` pins tool/time/token ceilings, cutoff and the review deadline.
The host must call `validate_settlement()` before a final verdict and enforce
expiry at read time: the current Nexus evaluator input does not contain the
EvaluationRecord cutoff. OutcomeRecord has no custom cost fields, so the trusted
verifier checks bounded success and retains measurements in Evidence payloads.
`workflow_contract()` supplies the first resettable task-family contract. See the
[frozen workflow contract](assets/learning/workflow-contract-v1.json).
The previous v1 evaluator digest is rejected rather than assigned these new semantics.

`Space::learning()` now provides explicit registration, frozen enrollment,
persistent dispatch/recovery and separately authenticated Outcome ingestion.
It uses native leases and dispatch authority checks; no executable authority or
Skill standing is assigned automatically. The host supplies the real executor
and observer, and calls the bounded `drive` step. Compilation does not deploy
those bindings. The host's `settle` step now performs
fixed-cutoff native comparison and atomic standing updates. Persistent reviews,
new monitoring trials, independently authenticated safety revocation, and current
read-time recommendation checks are implemented. Recall's internal read-only
`check_procedure_status` tool reports these checks and does not grant execution
authority. The [runtime implementation](src/learning/runtime.rs) exposes the trusted host interfaces.
The native tests use deterministic fixture outcomes to verify KIP transactions,
not to claim empirical learning. Run them without a model provider:

```bash
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features learning learning::
```

### Isolated experiments

With the Rust `experiments` feature, the host-only
`Experiment` owner supplies isolated stores, quiescent immutable snapshots,
per-conversation completion waits, monotonic business time, session boundaries,
and cost receipts with explicit unknown values. It enables Nexus's non-default
`simulation` host API for lifecycle expiry; authentication, lease and audit
clocks remain real. No HTTP/MCP clock override or MIB adapter is exposed.
Notes now persist under each Space's `engine/` object-store prefix and are
included in experiment snapshots. See the [experiment API](src/space/experiments.rs) and [MIB integration](#mib-integration).

`Experiment::create_with_recall_budget` pins a forced P5 policy before the run
is exposed, including across snapshot forks and session boundaries.
`audit_procedures()` supplies a bounded native inventory for evaluator-side
before/after checks. It does not establish applicability or execution permission.
See [P6 validation and remaining bindings](#mib-integration).

### MIB integration

The sibling Anda Bot `mib` feature provides an isolated loopback host before
production home/daemon initialization. Its agent endpoint is
`/mib-agent/v0.1`; its memory backend endpoint is `/mib-memory/v0.1`.
Each run has a separate store and business clock. Formation/Maintenance wait
for exact terminal records, repeated requests preserve their original result,
and current-task tool replies remain available in no-memory mode. See the
[Bot host contract](https://github.com/ldclabs/anda-bot/blob/main/docs/mib-integration.md)
and [MIB backend contract](https://github.com/ldclabs/MIB/blob/main/docs/harness/MIB-Memory-Backend.md).

P6's [longitudinal harness](https://github.com/ldclabs/MIB/blob/main/docs/harness/MIB-Learning-Longitudinal.md)
requires explicit normal/no-memory/ungated capabilities, matched business
identities and fixed budgets. The current Bot provides persistent/no-memory
modes; native normal/ungated business bindings remain pending. Unknown costs
stay unknown, and engineering fixtures never establish model-learning success.
The evaluator-only `learning_audit` reads bounded native inventories (256 items
per kind, 4 MiB total projection); an incomplete count cannot prove absence.
It never grants applicability or execution permission and is not fed back to
Formation. Complete accounting and real provider calibration are separate
acceptance work.

**Task work requires a lease.** The internal tool's `lease_task` operation acquires
or renews a five-minute lease under the runtime Principal. After re-reading the
version, maintenance commits terminal task state and outputs in one guarded MUTATE.
WatchState and LeaseState cannot be written by model KML. There is no external
dispatch adapter. Runtime authentication, tool capabilities and evaluator code never
come from model-generated content.

Operational records written against CognitiveMemory 2.0 cannot be armed or leased
in place: their exact `schema_ref` is immutable. Maintenance must create a 2.1
replacement, reconnect and verify its structural references, and only then archive
the legacy Watch or SleepTask. The runtime returns this migration instruction before
calling the protected operation.

**Current basis matters.** Recall loads a fresh Primer because policy, trust and
identity can change independently of vocabulary. A stored WorkingState/DerivationState
or bare `basis_seq` cannot override computed dependency validity. Derived refreshes
must retain their actual read pins, context and ProjectionBasis; incomplete coverage
or unavailable replay material remains explicit. Full KIP syntax is available to
writing agents through `memory_runtime` operation `syntax`; routine context uses
the upstream role cards and ontology.

**Retention expiry:** a full settlement also acts on the two clocks that say
when something should stop being kept, which are not the same clock. An
Assertion whose `valid_time.until` has passed is marked `expired` (§14.3) —
not retracted and not superseded, because nobody withdrew it and nothing
replaced it. An element whose `retention.expires_at` has passed is archived:
out of ordinary recall, still readable, still referenced. Purge is
deliberately unreachable from here — erasure over a set nobody enumerated is
the largest irreversible action this service can take, and a scheduled cycle
is not where that decision belongs; `POST /memory/forget` enumerates its
target and purges that. A legal hold stops the sweep that authorized it, and
the `retention` block of the settlement report says how many were held,
refused and left for the next cycle rather than reporting only what it
managed to archive.

> **Known scale ceiling:** the bulk decay, correction discovery, and
> self-test sampling passes use unconstrained full-scan KQL, and the engine
> caps full-scan solutions at 65,536 regardless of `LIMIT`.
> On graphs past ~65k propositions these passes stop working; the failure
> is loud (`log::error` + `decay_error`/`correction_scan_error` in the
> settlement report and `memory_status`), but the fix — predicate-sharded
> scans — is not implemented yet. Watch those report fields in production.

> **Single writer per space:** the usage ledger, settlement, self-test,
> shadow, and negative-cache locks are in-process (`tokio::Mutex` /
> atomics), like the formation processing flag they follow. Sharding
> assigns each space to exactly one process — do not point two instances
> at the same space's storage: concurrent ledger writes can duplicate
> rows, and the miss-cache clear race guard does not cross processes.
> (The graph-side fences — `decay_applied_at`, `correction_settled` —
> stay safe either way.)

**Dream self-test (self-repair):** after each maintenance cycle completes,
the runtime samples recent memories with no usage evidence, generates one
natural probe query per memory (a single LLM call, budgeted by
`MemoryPolicy.self_test_queries_per_cycle`), and checks deterministically
whether search actually surfaces them. Unfindable memories become pending
`review` SleepTasks (source `memory_self_test`) that the next full cycle
re-encodes with aliases and richer descriptions. Self-test retrievals count
only into the ledger's isolated `self_test_count` — the brain testing itself
never reinforces its own memories. The pass report lives in the
`memory_self_test` extension and surfaces as the `groundability` graph stat.

**Metamemory:** `search_exhaustive` reports optional search-window coverage;
missing coverage is unknown. A search miss is not a negative BELIEF, and only
explicit exhaustive misses enter the cache. `POST /v1/{space_id}/probe` answers "do I know anything
about this?" with pure search — no LLM, no recall cost. Queries that find
nothing are remembered in a negative-knowledge cache (cleared whenever
formation completes, 1h TTL backstop), so agents stop paying to hit the same
wall. The intended contract: probe first, and only pay for a full recall
when `found` is true.

**Memory observability:** `GET /v1/{space_id}/memory_status` returns
incrementally-maintained counters (recalls, probe hits/misses, self-test
groundability, corrections, decay, forget) plus derived rates
(probe hit rate, correction rate, mean self-reported uncertainty,
maintenance tokens per recall — the memory-ROI proxy), graph counts
including the `predicate_types` schema-sprawl indicator, and the latest
settlement / self-test / shadow reports. Writers bump counters at write
time; reading the status never runs heavy queries. Full-scope settlements
also refresh a per-predicate link census, reported as `last_schema_audit`
and handed to the Maintenance prompt as `assessment.predicates` — with the
per-actor correction tallies as `assessment.source_reliability` — so the
predicate-merge and contradiction guidance has real numbers rather than the
model's impression of them.

**Shadow evaluation (safe policy canary):**
`POST /v1/{space_id}/management/shadow_eval` compares a candidate
`MemoryPolicy` against the current one on the **production distribution**:
the space is forked twice into isolated in-memory stores (baseline vs
candidate policy), both forks are settled, recent real recall queries are
replayed on each, and the judge blind-compares the answers with
deterministic A/B order alternation. The live space is only read — replays
can never pollute its conversations, usage ledger, or metrics. The report
(wins/ties/samples/usage) persists in the `shadow_report` extension;
promotion stays human: read the report, then `update_space` with the
candidate policy if it won.

## Offline regression and instance configuration

The Rust `anda_brain::eval` API and the `anda_brain eval` CLI, including
`--optimize`, `--mine`, validation/report modes and their fixture profiles,
have been retired. MIB owns the migrated product regressions. Brain retains
its online self-test, `/probe`, citations/metadata, usage and correction ledgers,
shadow diagnostics, isolated experiment controls and native learning runtime.
The separate wiki retrieval corpus remains at `evals/wiki/retrieval.json`.

From the sibling MIB checkout, run the public regression contracts without a model:

```bash
python scripts/check-brain-product-regression.py --output-dir /tmp/brain-product-regression
```

For a configured business Agent, use MIB's normal submission path:

```bash
python -m mib_runner benchmark \
  --profile profiles/MIB-Brain-Product-Regression-0.1-Dev.json \
  --schema schemas/mib-scenario.schema.json \
  --submission /absolute/path/agent.json \
  --output-report /tmp/product-real.report.json
python -m mib_runner verify-score /tmp/product-real.report.json
```

The contract fixture verifies the harness and its oracle, not model quality.
P6's actual normal/ungated learning bindings and three-arm provider runs remain
separate pending work. The [MIB migration guide](https://github.com/ldclabs/MIB/blob/main/docs/harness/MIB-Brain-Legacy-Migration.md)
records the nine product goals, removed Rust APIs and validation evidence.

Memory policies come only from each Space's persisted `MemoryPolicy` or the
compiled defaults. `UpdateSpaceInput.memory_policy` remains the configuration
entry point; there is no process-global policy override. Trusted Rust hosts can
configure deployment prompts before sharing an `AppState` or opening a Space:

```rust
use anda_brain::agents::prompts::{AgentPrompts, PromptTarget};

let prompts = AgentPrompts::default().with_deployment_section(
    PromptTarget::Recall,
    "# A. Deployment contract\nYour reviewed deployment instructions here.",
)?;
let app = app.with_agent_prompts(prompts)?;
```

The supplied text replaces only section A, is limited to 128 KiB, and must start
with `# A.`. The compiled KIP reference prefix is retained verbatim. The prompt
configuration is immutable and inherited by agent instances and isolated forks;
snapshot identity checks include its actual contents. `active_prompt()` now
returns compiled defaults only. No HTTP/MCP prompt mutation endpoint is added.
Prompt configuration is host-owned and must be supplied again at startup;
per-Space `MemoryPolicy` remains persisted.

## API Endpoints

Detailed API docs (with TypeScript request/response types):
- English: [API.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API.md)
- 中文: [API_cn.md](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/API_cn.md)
- Agent Skill: [SKILL.md](https://github.com/ldclabs/anda-brain/blob/main/skills/anda-brain/SKILL.md)

| Method  | Path                                                   | Description                                                                   | Auth Scope                   |
| ------- | ------------------------------------------------------ | ----------------------------------------------------------------------------- | ---------------------------- |
| `GET`   | `/`                                                    | Anda Brain website                                                            | —                            |
| `GET`   | `/favicon.ico`                                         | Favicon                                                                       | —                            |
| `GET`   | `/apple-touch-icon.webp`                               | Apple touch icon                                                              | —                            |
| `GET`   | `/info`                                                | Service info (name, version, sharding)                                        | —                            |
| `GET`   | `/SKILL.md`                                            | Skill description (Markdown)                                                  | —                            |
| `GET`   | `/v1/{space_id}/info`                                  | Get space status & statistics                                                 | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/formation_status`                      | Get formation status (lightweight endpoint for monitoring formation progress) | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/formation`                             | Submit messages for memory encoding                                           | `write` (CWT or space token) |
| `POST`  | `/v1/{space_id}/recall`                                | Query memory with natural language                                            | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/recall_structured`                     | Recall with machine-readable provenance (citations, found, uncertainty)       | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/probe`                                 | LLM-free metamemory existence check with negative-knowledge caching           | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/memory/pin`                            | Pin/unpin a memory (pinned memories are exempt from confidence decay)         | `write` (CWT or space token) |
| `POST`  | `/v1/{space_id}/memory/forget`                         | Privacy-grade deletion (dry-run supported; physically removes, not archives)  | `write` (CWT or space token) |
| `GET`   | `/v1/{space_id}/memory_status`                         | Memory observability: usage/probe/self-test counters, rates, graph counts    | `read` (CWT or space token)  |
| `POST`  | `/v1/{space_id}/management/shadow_eval`                | Compare a candidate memory policy on forked copies (recent-recall replay)     | `write` (CWT)                |
| `POST`  | `/v1/{space_id}/maintenance`                           | Trigger maintenance cycle                                                     | `write` (CWT or space token) |
| `POST`  | `/v1/{space_id}/execute_kip_readonly`                  | Execute a KIP request (read-only mode, suitable for queries)                  | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations/{conversation_id}`       | Get one conversation detail                                                   | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations/{conversation_id}/delta` | Get incremental conversation updates                                          | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/conversations`                         | List conversations (cursor pagination)                                        | `read` (CWT or space token)  |
| `GET`   | `/v1/{space_id}/management/space_tokens`               | List space tokens                                                             | `write` (CWT)                |
| `POST`  | `/v1/{space_id}/management/add_space_token`            | Add a space token                                                             | `write` (CWT)                |
| `POST`  | `/v1/{space_id}/management/revoke_space_token`         | Revoke a space token                                                          | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/update_space`               | Update space information (name, description, public/private, memory policy)   | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/restart_formation`          | Restart a formation task                                                      | `write` (CWT)                |
| `GET`   | `/v1/{space_id}/management/space_byok`                 | Get BYOK (Bring Your Own Key) configuration                                   | `write` (CWT)                |
| `PATCH` | `/v1/{space_id}/management/space_byok`                 | Update BYOK (Bring Your Own Key) configuration                                | `write` (CWT)                |
| `POST`  | `/admin/{space_id}/update_space_tier`                  | Update a space tier (manager only)                                            | `write` (CWT)                |
| `POST`  | `/admin/create_space`                                  | Create a new space (manager only)                                             | `write` (CWT)                |

### MCP Server

When the HTTP service starts, Anda Brain also exposes a Streamable HTTP MCP endpoint:

```text
https://your-brain-host/mcp/{space_id}
```

Use this for multi-user deployments where each employee or agent team receives a dedicated Brain space. MCP clients should send the same CWT or space token used by REST as `Authorization: Bearer <token>`. Read-only tools can access public spaces without a token.

For local desktop or development clients, Anda Brain can also run as a stdio MCP server:

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  cargo run -p anda_brain --features mcp,wiki -- mcp --space-id my_space_001 local --db ./data
```

Both MCP modes use the same storage/model configuration as the HTTP service and expose these tools:

| Tool | Purpose | Scope |
| ---- | ------- | ----- |
| `anda_brain_remember_conversation` | Encode conversation messages into memory | `write` |
| `anda_brain_recall_memory` | Ask natural-language questions against memory | `read` |
| `anda_brain_run_maintenance` | Trigger memory consolidation/pruning | `write` |
| `anda_brain_get_space_info` | Read space statistics and metadata | `read` |
| `anda_brain_get_formation_status` | Read formation/maintenance progress | `read` |
| `anda_brain_execute_kip_readonly` | Run read-only KIP for advanced graph inspection | `read` |
| `anda_brain_get_or_init_user` | Get or create a counterparty concept | `write` |
| `anda_brain_list_conversations` | Page through tracked conversations | `read` |
| `anda_brain_get_conversation` | Read one tracked conversation or delta | `read` |

If authentication is enabled, pass a CWT or space token. Remote MCP reads it from the HTTP `Authorization` header; stdio reads it from `MCP_AUTH_TOKEN` or `--mcp-auth-token`. For local-only development with auth disabled, the token can be omitted. Remote MCP auto-create requires `ED25519_PUBKEYS` plus a `write` CWT for the target space before the missing space is created. Use `MCP_HTTP_ALLOWED_HOSTS` when exposing remote MCP behind a company domain or reverse proxy.

### Content Negotiation

Triple serialization via `Content-Type` / `Accept` headers:

- `application/json` — JSON (default)
- `application/cbor` — CBOR (binary, more compact)
- `text/markdown` — Markdown (human-readable text)

All responses use an RPC envelope:

```json
{"result": { ... }, "error": null}
```

### Authentication

All endpoints (except `/`, `/info` and `/SKILL.md`) require a Bearer token:

```
Authorization: Bearer <base64_encoded_cose_sign1_token>
```

If `ED25519_PUBKEYS` is not provided (empty), authentication is effectively disabled: API requests are accepted without signature verification.

Token format: COSE Sign1 message signed with Ed25519 keys, containing CWT claims:

| Claim   | Purpose                                                 |
| ------- | ------------------------------------------------------- |
| `sub`   | Principal ID (who is making the request)                |
| `aud`   | Audience — the space ID being accessed (or `*` for any) |
| `scope` | Permission level: `read`, `write` (or `*` for any)      |

### POST /admin/create_space

Create a new isolated memory space. Requires manager principal.

**Request:**
```json
{
  "user": "<owner_principal_id>",
  "space_id": "my_space_001",
  "tier": 0
}
```

**Response:**
```json
{
  "result": {
    "space_id": "my_space_001",
    "owner": "owner_principal_id",
    ...
  }
}
```

### POST /v1/{space_id}/formation

Submit conversation messages for memory encoding. Processing is asynchronous — returns immediately while encoding continues in the background.

**Request:**
```json
{
  "messages": [
    {
      "role": "user",
      "content": "I prefer dark mode. My timezone is UTC+8.",
      "name": "Alice"
    },
    {
      "role": "assistant",
      "content": "Got it! I've noted your preferences."
    }
  ],
  "context": {
    "counterparty": "alice_principal_id",
    "agent": "customer_bot_001",
    "source": "source_123",
    "topic": "settings"
  },
  "timestamp": "2026-03-09T10:30:00Z"
}
```

| Field                  | Type        | Required | Description                                                     |
| ---------------------- | ----------- | -------- | --------------------------------------------------------------- |
| `messages`             | `Message[]` | Yes      | Conversation messages (`role`: `user` / `assistant` / `system`) |
| `context.counterparty` | `string`    | No       | User identifier                                                 |
| `context.agent`        | `string`    | No       | Calling agent identifier                                        |
| `context.source`       | `string`    | No       | Identifier of the source of the current interaction content     |
| `context.topic`        | `string`    | No       | Conversation topic                                              |
| `timestamp`            | `string`    | No (recommended) | ISO 8601 timestamp                                      |

**Response:**
```json
{
  "result": {
    "conversation": 1,
    ...
  }
}
```

### POST /v1/{space_id}/recall

Query memory with natural language. Returns a synthesized answer from the knowledge graph and conversation history.

**Request:**
```json
{
  "query": "What are Alice's preferences?",
  "context": {
    "counterparty": "alice_principal_id",
    "topic": "settings"
  }
}
```

| Field                  | Type     | Required | Description                   |
| ---------------------- | -------- | -------- | ----------------------------- |
| `query`                | `string` | Yes      | Natural language question     |
| `context.counterparty` | `string` | No       | User identifier               |
| `context.agent`        | `string` | No       | Calling agent identifier      |
| `context.topic`        | `string` | No       | Topic hint for disambiguation |

**Response:**
```json
{
  "result": {
    "content": "Alice prefers dark mode and operates in UTC+8 timezone.",
    ...
  }
}
```

### POST /v1/{space_id}/maintenance

Trigger a memory maintenance cycle. Runs asynchronously with single-execution guard.

**Request:**
```json
{
  "trigger": "on_demand",
  "scope": "daydream",
  "timestamp": "2026-03-10T03:00:00Z",
  "parameters": {
    "stale_event_threshold_days": 7,
    "memory_strength_decay_factor": 0.95,
    "unconsolidated_max_backlog": 20,
    "orphan_max_count": 10
  }
}
```

| Field                                   | Type     | Required | Description                                               |
| --------------------------------------- | -------- | -------- | --------------------------------------------------------- |
| `trigger`                               | `string` | No       | `scheduled` / `threshold` / `on_demand` (default: `on_demand`) |
| `scope`                                 | `string` | No       | `full` (all phases) / `quick` (assessment + urgent tasks) / `daydream` (idle-time salience scoring & micro-consolidation, default) |
| `timestamp`                             | `string` | No       | ISO 8601 timestamp                                        |
| `parameters.stale_event_threshold_days` | `u32`    | No       | Days before events are considered stale (default: 7)      |
| `parameters.memory_strength_decay_factor` | `f64`  | No       | Multiplier disuse metabolism applies to `MnemonicState.memory_strength` (default: 0.95). Never to `confidence`: KIP 2.0 forbids letting time erode a stance. Accepted under its KIP 1.x name `confidence_decay_factor` for stored-policy compatibility. |
| `parameters.unconsolidated_max_backlog` | `u32`    | No       | Events and Experiences that may sit without `consolidated_to` lineage (default: 20). Accepted as `unsorted_max_backlog`. |
| `parameters.orphan_max_count`           | `u32`    | No       | Max orphans to process (default: 10)                      |

The parameters are targets to work toward, not commands. The runtime fills an
`assessment` block into the same input — the per-predicate census, correction
tallies, armed and fired Watches, and the current `space_seq` — and overwrites
whatever a caller sent there: a request body must not be able to tell the Brain
what its own graph looks like.

**Response:**
```json
{
  "result": {
    "conversation": 8,
    ...
  }
}
```

### GET /v1/{space_id}/info

Get space statistics and health information.

**Response:**
```json
{
  "result": {
    "space_id": "my_space_001",
    "owner": "principal_id",
    "db_stats": { "total_items": 150, "total_bytes": 524288 },
    "concepts": 85,
    "propositions": 120,
    "conversations": 12,
    ...
  }
}
```

## Recall Function Definition

Business agents can register the Recall endpoint as an LLM tool/function call. See [RecallFunctionDefinition.json](https://github.com/ldclabs/anda-brain/blob/main/anda_brain/assets/RecallFunctionDefinition.json) for the OpenAI function-calling format.

## Memory Space Lifecycle

### Creation
1. Creates a new `AndaDB` instance.
2. Initializes `CognitiveNexus`.
3. Activates the KIP 2.0 Cognitive Memory Profile plus this space's own vocabulary package.
4. Stores creator/owner principal IDs.

### Upgrading a space written by a KIP 1.x build

Migration runs when a space is first opened, after the Brain activates its
Schema. Stop the old writer, take a consistent backup, and rehearse against a
copy before changing production. The two old graph collections are replaced in
place; rollback requires the pre-upgrade backup, not an older binary pointed at
the migrated store. Upgrade one space at a time and check its results before
opening the next.

The migration persists extraction and vocabulary checkpoints before switching
collections. It can resume between either collection deletion and between
loading records and committing the completion marker. Original rows remain in
`kip_legacy_v1`. `LegacyRecord` Facets preserve source data for audit, including
the published v1 `a` / `m` attribute and metadata fields.

| v1 data | v2 representation |
| --- | --- |
| `(type, name)` identity | Immutable Concept `key`; standard Person / Event / Preference / Insight / Commitment / SleepTask fields are normalized |
| Insight `description`; SleepTask `reason` / `requested_action` | Native summary / task class; interrupted tasks become blocked without a fabricated lease |
| A recorded claim | Proposition + `mode: "imported"` Assertion; recorded confidence and resolvable author are preserved |
| Retracted or superseded claim | Native lifecycle where the same-actor, same-Proposition revision is reconstructible; otherwise archived with its original annotations, never revived as current belief |
| `valid_from` / `valid_until`; `expires_at` | Assertion valid time; record retention, respectively |
| `pinned`; mnemonic values | Pinned retention class; `MnemonicState`, never copied from Assertion confidence |
| Unsupported old learning/runtime artifacts | Distinct `Legacy*` types under `kip://legacy/nexus@1.1.0`; they acquire neither learning standing nor operational leases |
| Legacy relation with incompatible native endpoints | A distinct legacy predicate for that tuple; compatible tuples keep the native predicate |

Unresolvable attribution and privacy annotations remain source data, not
verified identity, trust or Governance permission. Invalid optional native
values remain available in `LegacyRecord`. Malformed identifiers or dangling
references can still stop migration; inspect the reported source row and retry
on the backup copy rather than treating an unopened space as empty memory.

The service removes old-id usage rows and resets miss caches, derived metrics
and scan cursors once. It preserves conversations, tokens, policies, wiki
records and any already-recorded v2 usage. The migration is tested against a
complete object-store snapshot produced by the published v0.11 runtime packages;
see [the fixture generator](../scripts/fixtures/v0_11/README.md).

### Runtime
- Spaces are **lazy-loaded** on first access via `OnceCell`.
- In-memory cache with access tracking.
- **5-minute interval**: Flush active spaces to storage.
- **9-minute idle timeout**: Evict unused spaces from cache (skipped while a space is pinned, processing, or still referenced by requests).
- Graceful shutdown: Close all space databases before exit.

### Memory Elements in the Cognitive Nexus

KIP 2.0 separates things KIP 1.x kept in one graph. The distinction the rest
follows from is that **a Proposition existing is not the Proposition being
true**:

| Element         | What it is                                                                      |
| --------------- | ------------------------------------------------------------------------------- |
| **Concept**     | A referable entity: `schema_ref`, immutable `key`, mutable `name`, attributes    |
| **Proposition** | A truth-neutral `(subject, predicate, object)` tuple                            |
| **Assertion**   | One actor's stance about a Proposition: `asserted_by`, `mode`, `confidence`     |
| **Evidence**    | An observed artifact — a message, a tool result, a document passage             |
| **Activity**    | The provenance of a process: consolidation, revision, import                    |

What is *currently believed* is projected from Assertions under a named policy
(`BELIEF`), never stored. Mnemonic state — how available and how noteworthy a
memory is — lives in the `MnemonicState` Facet, and is not confidence.

**Schema is not graph state.** Types and predicates are resolved from immutable,
versioned Schema Packages, so a write cannot change what a type means. This
space activates the standard [Cognitive Memory
Profile](https://github.com/ldclabs/KIP) plus a `kip://anda-brain/memory`
package of its own. When Formation meets vocabulary the profile lacks, it asks
the host to publish it (`declare_memory_symbols`) — the host validates the
name's shape, caps how many a space may hold, and versions the result.

## Configuration

### CLI Arguments / Environment Variables

| Env Variable           | CLI Flag                 | Default                             | Description                                                                          |
| ---------------------- | ------------------------ | ----------------------------------- | ------------------------------------------------------------------------------------ |
| `LISTEN_ADDR`          | `--addr`                 | `127.0.0.1:8042`                    | Listen address                                                                       |
| `ED25519_PUBKEYS`      | `--ed25519-pubkeys`      | —                                   | Comma-separated Base64 Ed25519 public keys; if empty, API authentication is disabled |
| `MODEL_FAMILY`         | `--model-family`         | `anthropic`                         | Model family to use for encoding and recall (e.g., `gemini`, `anthropic`, `openai`)  |
| `MODEL_API_KEY`        | `--model-api-key`        | —                                   | API key for the configured model provider                                            |
| `MODEL_API_BASE`       | `--model-api-base`       | `https://api.deepseek.com/anthropic` | Model API base URL                                                                  |
| `MODEL_NAME`           | `--model-name`           | `deepseek-v4-pro`                   | LLM model for agents                                                                 |
| `MODEL_CONTEXT_WINDOW` | `--model-context-window` | `400000`                            | Model context window size (tokens)                                                   |
| `MODEL_MAX_OUTPUT`     | `--model-max-output`     | `384000`                            | Model max output size (tokens)                                                       |
| `HTTPS_PROXY`          | `--https-proxy`          | —                                   | HTTPS proxy URL                                                                      |
| `SHARDING_IDX`         | `--sharding-idx`         | `0`                                 | Shard index for this instance                                                        |
| `MANAGERS`             | `--managers`             | —                                   | Comma-separated manager principal IDs                                                |
| `CORS_ORIGINS`         | `--cors-origins`         | —                                   | CORS allowed origins: empty = disabled, `*` = allow all, or comma-separated origins  |
| `MCP_HTTP_ENABLED`     | `--mcp-http-enabled`     | `true`                              | Mount Streamable HTTP MCP with the HTTP service                                      |
| `MCP_HTTP_PATH_PREFIX` | `--mcp-http-path-prefix` | `/mcp`                              | Remote MCP prefix; clients connect to `{prefix}/{space_id}`                          |
| `MCP_HTTP_ALLOWED_HOSTS` | `--mcp-http-allowed-hosts` | —                                | Comma-separated Host allowlist for remote MCP; use `*` only behind trusted controls  |
| `MCP_HTTP_ALLOWED_ORIGINS` | `--mcp-http-allowed-origins` | —                            | Comma-separated browser Origin allowlist for remote MCP                              |
| `MCP_HTTP_AUTO_CREATE_SPACE` | `--mcp-http-auto-create-space` | `false`                    | Create remote MCP spaces on first use after a valid `write` CWT                      |
| `MCP_HTTP_AUTO_CREATE_TIER` | `--mcp-http-auto-create-tier` | `1`                         | Tier used for remote MCP auto-created spaces                                         |
| `MCP_SPACE_ID`         | `mcp --space-id`         | —                                   | Space exposed by the MCP stdio server                                                |
| `MCP_AUTH_TOKEN`       | `mcp --mcp-auth-token`   | —                                   | CWT or space token used by MCP tools                                                 |
| `MCP_AUTO_CREATE_SPACE` | `mcp --mcp-auto-create-space` | `false`                       | Create the MCP space if it does not exist                                            |
| `MCP_AUTO_CREATE_TIER` | `mcp --mcp-auto-create-tier` | `1`                            | Tier used for MCP auto-created spaces                                                |

`CORS_ORIGINS` examples:
- `""` (empty): CORS disabled
- `"*"`: allow all origins
- `"https://app.example.com,https://admin.example.com"`: allow specific origins

### Storage Backends

| Subcommand | Description                     | Key Env Variables                                                        |
| ---------- | ------------------------------- | ------------------------------------------------------------------------ |
| *(none)*   | In-memory storage (dev/testing) | —                                                                        |
| `local`    | Local filesystem storage        | `LOCAL_DB_PATH` (default `./db`)                                         |
| `aws`      | AWS S3 storage                  | `AWS_BUCKET`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` |

## Cargo Features

The library ships memory only by default — formation, recall, maintenance, and
their HTTP routes. Everything else is opt-in:

| Feature | Adds                                                                                                                                                                                    |
| ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `wiki`  | The structured wiki (documents, versions, ACL-scoped reads, OKF import/export), the `wiki_search`/`wiki_read`/`wiki_commit` agent tools, WikiDigest graph extraction, and the `/v1/{space_id}/wiki/*` routes. Also adds the `wiki_*` fields of `SpaceInfo` and `UpdateSpaceInput`. |
| `mcp`   | The MCP channel: the stdio server and the Streamable HTTP service. With `wiki` on as well, the wiki tools join the MCP tool router.                                                        |
| `experiments` | Isolated Rust host runs, immutable snapshots, business time, session boundaries and cost receipts. Enables Nexus `simulation`, without changing production clocks or enabling learning. |
| `learning` | Paired contracts/evaluator, native records, persistent host trial/settlement/review runtime, and a read-only Recall applicability tool. Explicit executor/observer/current-context bindings are required; no model standing writes or production scheduler is installed. |

```toml
# Embedding the library: memory only
anda_brain = "0.12"

# …or the full surface
anda_brain = { version = "0.12", features = ["mcp", "wiki"] }
```

To use development APIs before a crate release, build this checkout. Its KIP
2.0 dependencies resolve from published crates, so no sibling checkout is
needed. Trusted experiment hosts add `experiments`; it is independent of the
`learning` feature.

The `anda_brain` **binary** is the full product and declares
`required-features = ["mcp", "wiki"]`, so every command below passes
`--features mcp,wiki`. Without them Cargo skips the binary target.

## Running

```bash
# Development (in-memory storage)
cargo run -p anda_brain --features mcp,wiki

# Local filesystem storage
cargo run -p anda_brain --features mcp,wiki -- local --db ./data

# HTTP service also serves remote MCP at /mcp/{space_id}
MCP_HTTP_ALLOWED_HOSTS="brain.example.com" \
  cargo run -p anda_brain --features mcp,wiki -- local --db ./data

# AWS S3 storage
cargo run -p anda_brain --features mcp,wiki -- aws --bucket my-bucket --region us-east-1

# MCP stdio server for local MCP clients
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  cargo run -p anda_brain --features mcp,wiki -- mcp --space-id my_space_001 local --db ./data
```

### Run with Docker image

```bash
# Pull image
docker pull ghcr.io/ldclabs/anda_brain_amd64:latest

# Run with ENV (in-memory by default)
docker run --rm -p 8042:8042 \
  -e LISTEN_ADDR=0.0.0.0:8042 \
  -e MODEL_API_KEY=your_key \
  ghcr.io/ldclabs/anda_brain_amd64:latest

# Override startup args (example: local storage)
docker run --rm -p 8042:8042 \
  -v $(pwd)/data:/data \
  ghcr.io/ldclabs/anda_brain_amd64:latest local --db /data

# Override startup args (example: AWS S3 storage)
docker run --rm -p 8042:8042 \
  -e AWS_ACCESS_KEY_ID=your_ak \
  -e AWS_SECRET_ACCESS_KEY=your_sk \
  ghcr.io/ldclabs/anda_brain_amd64:latest aws --bucket my-bucket --region us-east-1
```

## Dependencies

Key crates from the Anda ecosystem:

| Crate                  | Purpose                                                        |
| ---------------------- | -------------------------------------------------------------- |
| `anda_core`            | Core traits (`Agent`, `Tool`, `AgentContext`) and types        |
| `anda_engine`          | Agent engine, model integration, memory management             |
| `anda_db`              | Persistent database layer (`AndaDB`) with configurable storage |
| `anda_kip`             | KIP 2.0 protocol: parser, error registry, request envelope     |
| `anda_cognitive_nexus` | Cognitive Nexus knowledge graph implementation                 |
| `object_store`         | Object store abstraction                                       |

## License

Copyright © LDC Labs

Licensed under the Apache License, Version 2.0.
