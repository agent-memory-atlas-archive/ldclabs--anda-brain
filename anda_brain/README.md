# Anda Brain — Technical Documentation

A dedicated LLM-powered memory management service that maintains a persistent **Cognitive Nexus** on behalf of business AI agents via [KIP 2.0 (Knowledge Interaction Protocol)](https://github.com/ldclabs/KIP).

Business agents interact entirely through natural language and a REST API — no KIP knowledge required.

Anda Brain is designed to be **self-hosted** (the hosted cloud service has been discontinued). For a complete agent built on Anda Brain, see [Anda Bot](https://github.com/ldclabs/anda-bot).

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
- **Longitudinal eval harness** — Development/CI support for replaying user timelines, probing graph state, scoring checkpoints, and attributing memory failures to Formation, Recall, or Maintenance.

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
4. **Procedural consolidation** — Compare successful against failed Experiences and compile a `proposed` Skill with the `task_family` that can grade it. A pattern no outcome stream could prove wrong is an Insight, not a Skill.
5. **Identity review** — Review `same_as` suspicions, then `MERGE CONCEPT`, which is non-destructive: the source survives as merged historical identity.
6. **Contradiction and derivation review** — Different actors' disagreement coexists; only an actor's own revision supersedes. After a revision, walk `LIST DEPENDENTS` and flag derived artifacts `stale` for review.
7. **Mnemonic metabolism** — run by the runtime settlement before the cycle, not by the agent: `MnemonicState.memory_strength * decay_factor` on Concepts due for it. Never `confidence`; a fact nobody has asked about lately is no less credible. `salience` and `utility` stay with the agent — the sweep cannot make a per-memory judgement.
8. **Commitments, Watches and the action gate** — Review what is owed and what is being waited for. The runtime has already fired the `silence` Watches whose deadline passed; the agent evaluates `delta` Watches against `CHANGES AFTER SEQ` and records what it decided about each fired one as an `action_gate` outcome (`act` / `ask` / `defer` / `silence`). See "Waiting is active" below.
9. **SelfModel and WorkingState refresh** — Consolidate identity from evidence rather than from the latest conversation, and rebuild the digest the next waking session resumes from, stamped with the `basis_seq` it was built at.
10. **Retention review** — Decide what should carry an expiry and write it with `SET RETENTION`; the full settlement's sweep is what makes that write mean something. See "Retention expiry" below.

Skill lifecycle transitions are deliberately absent from this list: they are deterministic code, not agent work. See "The Skill lifecycle is code, not a prompt" below.

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
miner read. That record stops there. An earlier design closed the loop —
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

**Waiting is active:** every settlement fires the `silence` Watches whose
`due_at` has passed — the transition to `fired` and its `watch_fire` Activity,
atomically and guarded by `EXPECT VERSION`. A deadline arriving is arithmetic,
and arithmetic should not wait for a model to be scheduled, have context budget
and notice. Firing produces attention and nothing else: no SleepTask, no
outward act, and no `action_gate` outcome, because `act` / `ask` / `defer` /
`silence` are all judgements about what the deadline *means*. Those wait in
`assessment.fired_watches` for the maintenance cycle. `delta` Watches stay with
the model: matching a condition written in prose is interpretation, not
arithmetic.

**The Skill lifecycle is code, not a prompt.** `proposed → trialed → adopted →
revoked` moves only by deterministic verdict over Outcome Evidence — Profile
§14 rule 1: "the Brain proposes, compiles, and narrates; it never promotes."

Which outcomes count is rule 7, *attribution before counting*: an outcome grades
a Skill only when it is **linked** to a decision that applied it — an
`action_gate` Activity naming the Skill among its `inputs`, and the instrument's
`outcome_observation` Activity naming that gate among its own. An outcome that
merely shares the `task_family` is the **baseline** the trial is measured
against, never a grade, so two Skills in one family are judged by their own runs
rather than by each other's.

Every transition commits as a `lifecycle_verdict` Activity citing the linked
Evidence it read, with the rule identity pinned in `parameters_digest` and the
basis on the Skill's own `TrialState` — the coordinate the trial opened at, the
family's tallies excluding this Skill, and the quota of linked outcomes the rule
needs — so an auditor can re-run it from state alone. The tallies live in
`GradingState` and the revised admission bet in `MnemonicState.utility`: a
record of what happened is not a forecast, and neither is authority.

Adoption is comparative (better than the recorded baseline, not merely good) and
provisional (a rate that falls back demotes to a re-trial); revocation uses the
same margin in the other direction, plus the one asymmetry the Profile grants —
a single high-severity matching-condition failure may revoke. A lifecycle that
can only acquire cannot tell a habit from a superstition. The model's job is
§11: compile a `proposed` Skill from contrastive Experience, attach the
`task_family` its baseline comes from, and set the admission bet at
compilation.

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
> caps full-scan solutions at 65,536 (`KIP_4002`) regardless of `LIMIT`.
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

**Metamemory:** `POST /v1/{space_id}/probe` answers "do I know anything
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

## Longitudinal Evaluation

Anda Brain includes an eval-first harness in `anda_brain::eval`. It drives the
same deep interface used by real callers:

1. Replay normal turns through Formation.
2. Optionally trigger Maintenance by explicit turns or every N normal turns.
3. Run checkpoint turns through Recall.
4. Execute read-only KIP probes before each checkpoint.
5. Score answer utility, forgetting quality, graph health, uncertainty, latency,
   and token cost.
6. Attribute failures to `formation_miss`, `bad_consolidation`,
   `bad_grounding`, `bad_synthesis`, or `overconfidence`.

The harness is intended for local experiments, regression tests, and CI
benchmarks. It is not a public HTTP endpoint. See
[`evals/style_preference.json`](evals/style_preference.json) for a minimal
scenario shape.

Beyond the basic replay loop, the harness supports:

- **Sampling & variance** — `checkpoint_samples: N` in a profile (or
  `--checkpoint-samples N`) runs Recall N times per checkpoint and reports the
  mean score plus `total_stddev` (propagated through suite and experiment
  aggregates). Findings only count when they appear in a majority of samples.
  `--confidence-z Z` makes `--min-score` gate on `total - Z * stddev`, so a
  lucky single roll cannot pass CI.
- **LLM judge** — `"judge": "llm"` in a profile scores each answer against the
  checkpoint's `scoring_rubric` and the scenario's `hidden_profile`.
  Paraphrases count fully, and correct meta-references to superseded facts
  ("unlike your old BBQ preference…") are not penalized as stale. The judge
  also emits attributed findings and a per-checkpoint `satisfaction` signal.
  The lexical scorer remains the default for deterministic smoke runs.
- **Semantic probes** — an expectation may state an `assertion` in natural
  language ("an active, non-superseded BBQ preference for user_042") instead
  of hand-written KQL. The harness runs a semantic graph search and asks the
  judge whether the evidence shows the statement, so probes stay correct
  across valid encoding variations. Raw `probe` KQL remains as fallback.
  `search_threshold` (default 0.35) and `search_limit` (default 8) tune the
  search per expectation. A probe whose KIP request itself fails degrades to
  a `graph_probe_error` finding — the expectation is scored as unknown, not
  as a memory failure, and the run continues.
- **Noise pressure** — a scenario-level `noise` config deterministically
  inserts chit-chat turns between authored anchors (`between_turns`, `seed`,
  optional `corpus`), scaling a 6-turn script into a long timeline where
  Formation must keep the needle in a haystack. Noise turns count toward
  `maintenance_every_n_turns` exactly like real user turns, so enabling noise
  also increases auto-maintenance frequency — deliberately, so Maintenance
  has real material to metabolize.
- **Simulated users** — `"type": "simulated"` turns carry an `intent`; an
  eval-only user simulator writes the actual message from `hidden_profile`,
  the recent transcript, and the satisfaction trail, adapting its behavior
  when the memory system has been failing it. Reports include a
  `satisfaction_trajectory` — the survival-pressure signal.
- **Trajectory metrics** — aggregate scores weight later checkpoints more
  (an established memory failing late costs more than an early miss), and
  the aggregate `evolution_quality` compares late-half vs early-half
  checkpoint scores: above 0.5 means the system improved over the timeline.
  The trajectory value is informational — the aggregated `total` stays the
  weighted mean of checkpoint totals (each of which used its own
  checkpoint-level evolution estimate) and is not recomputed from it.
  `graph_health` reads real metabolism counters (unconsolidated backlog, orphans)
  via read-only KIP instead of probe execution success.
- **Shared-formation experiments** — `--shared-formation` (with multiple
  `--profile`) replays formation once per scenario, snapshots the space, and
  forks the snapshot into an isolated in-memory store per profile. Every
  maintenance policy is then measured on identical encoded memory, removing
  formation LLM variance as a confound — and the most expensive phase runs
  once instead of once per profile. Requires all user turns to precede the
  first checkpoint (validated); use the default interleaved mode otherwise.
- **Prompt & policy optimization** — `--optimize
  formation|recall|maintenance|auto|policy` runs an offline evolution loop
  with the eval suite as fitness. Prompt genomes get surgical find/replace
  edits from an optimizer LLM; the `policy` genome mutates the numeric
  `MemoryPolicy` knobs instead (1–3 bounded ±50% steps per generation,
  range-validated — cheaper to evaluate and safer to apply). Candidates must
  beat the baseline beyond the sampling noise band or they are reverted.
  Accepted prompts (`Brain*.md`), the accepted policy
  (`memory_policy.json`), and the full decision log are written to
  `--optimize-out` (default `./eval_optimize`) for human review — nothing is
  written back to `assets/`. Note: the noise band only covers Recall
  sampling variance (`checkpoint_samples`) — each generation re-runs
  formation, whose LLM variance is *not* in the band, so prefer more
  scenarios and samples over trusting a single close call.
- **Holdout gate (anti-overfitting)** — `--holdout-scenario <file>` (with
  `--optimize`) runs a held-out suite whenever train accepts a candidate: a
  train win that drops the holdout total more than `holdout_epsilon`
  (default 0.01) below its baseline is rejected as overfitting, and the
  per-generation holdout totals land in the optimize report.
- **Independent judge** — `--judge-model-name/-api-key/-api-base/-family`
  (env `JUDGE_MODEL_*`) route all judge completions (checkpoint scoring and
  semantic assertion probes) to a separate model, so judge scores stop
  sharing the evaluated system's blind spots. An empty API key keeps the
  old same-model behavior.
- **Scenario mining** — `--mine` (with `--space-id` pointing at an existing
  space) distills the space's correction ledger into new eval scenarios:
  each superseded memory plus its source-conversation excerpts is handed to
  an LLM that writes a correction-replay scenario (strictly parsed and
  validated like hand-written fixtures, obvious PII scrubbed from both LLM
  input and output). Results land in `--mine-out` (default
  `anda_brain/evals/mined/`, deliberately *outside* the auto-validated
  `evals/*.json` glob) and require human review before promotion into the
  train or holdout suites. This is how the fitness function grows toward
  the production failure distribution.
- **Hermetic runs & cleanup** — every run executes in freshly created,
  run-scoped spaces named `{space_id}_{profile}_{scenario}_{run_id}`
  (lowercased to AndaDB's `[a-z0-9_]` charset and capped at 64 chars with a
  hash suffix), so reruns never see memory left over from a previous run.
  These spaces are deleted from the object store once their report is
  collected — including when a scenario aborts — and pass `--keep-spaces` to
  keep them for post-mortem inspection (e.g. poking the graph with read-only
  KIP).

Scenario and profile JSON is parsed strictly: an unknown field (usually a
typo like `forbidden_terms` for `forbidden_answer_terms`) fails the load
instead of silently weakening the rubric.

Validate scenario/profile inputs without running models:

```bash
cargo run -p anda_brain --features mcp,wiki -- \
  eval \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --profile anda_brain/evals/default_profile.json \
  --validate-only \
  --summary-only
```

Run a scenario locally:

```bash
cargo run -p anda_brain --features mcp,wiki -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id style_eval \
  --scenario anda_brain/evals/style_preference.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/style_eval_report.json \
  local --db /tmp/anda-brain-eval-db
```

Run a small suite by repeating `--scenario`. The suite writes one
`EvalSuiteReport` with per-scenario reports plus aggregate score, usage, and
failure attribution:

```bash
cargo run -p anda_brain --features mcp,wiki -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_suite \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --scenario anda_brain/evals/preference_reversal.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/anda_brain_eval_suite.json \
  local --db /tmp/anda-brain-eval-suite-db
```

Compare maintenance policies by repeating both `--scenario` and `--profile`.
The experiment writes one `EvalExperimentReport` with `best_suite_id` and
ranked `comparisons`, so profiles can be compared by quality, findings, and
token cost:

```bash
cargo run -p anda_brain --features mcp,wiki -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_experiment \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --scenario anda_brain/evals/preference_reversal.json \
  --profile anda_brain/evals/no_maintenance_profile.json \
  --profile anda_brain/evals/default_profile.json \
  --profile anda_brain/evals/quick_profile.json \
  --output /tmp/anda_brain_eval_experiment.json \
  local --db /tmp/anda-brain-eval-experiment-db
```

Add `--summary-only` to any eval command to print a compact human-readable
summary instead of JSON. Omit it for artifacts intended for CI or downstream
analysis.

Use gates in CI to fail when aggregate quality falls below a floor. The report
is written before the command exits non-zero, and gated runs include a top-level
`gate` object with the criteria, pass/fail state, and failure messages:

```bash
cargo run -p anda_brain --features mcp,wiki -- \
  --model-api-key "$MODEL_API_KEY" \
  eval \
  --space-id memory_ci \
  --scenario anda_brain/evals/style_preference.json \
  --scenario anda_brain/evals/project_budget.json \
  --profile anda_brain/evals/default_profile.json \
  --output /tmp/anda_brain_ci_eval.json \
  --min-score 0.75 \
  --max-findings 3 \
  local --db /tmp/anda-brain-ci-eval-db
```

Report shapes:

- `EvalValidationReport`: emitted by `--validate-only`; contains `passed`, `planned_runs`, scenario/profile plans, and validation `issues` with `error` or `warning` severity.
- `EvalReport`: single scenario result; contains `scenario_id`, aggregate `score`, optional `total_stddev`, `attribution`, `usage`, `satisfaction_trajectory`, optional `gate`, and per-turn reports (with per-sample scores, probes, graph stats, and judge reasoning).
- `EvalSuiteReport`: one profile across multiple scenarios; contains `suite_id`, aggregate `score`, optional `total_stddev`, `attribution`, `usage`, optional `gate`, and child `reports`.
- `EvalExperimentReport`: multiple profiles across the same scenario set; contains `experiment_id`, aggregate `score`, optional `total_stddev`, `best_suite_id`, ranked `comparisons`, optional `shared_formation` reports, optional `gate`, and child `suites`.
- `EvalScore`: normalized `total`, `memory_utility`, `evolution_quality`, `uncertainty_calibration`, `forgetting_quality`, `graph_health`, `latency_penalty`, and `token_cost_penalty`.
- `AttributionSummary`: counts failures by `formation_miss`, `bad_consolidation`, `bad_grounding`, `bad_synthesis`, `overconfidence`, `graph_probe_error`, `latency_cost`, `token_cost`, and `judge_error`.
- `OptimizeReport`: emitted by `--optimize`; contains `baseline_total`, `final_total`, per-generation edits with accept/reject decisions, and `accepted_prompts`.

Included starter scenarios:

- [`evals/style_preference.json`](evals/style_preference.json) — long-term writing style preference.
- [`evals/project_budget.json`](evals/project_budget.json) — project context and rough budget recall.
- [`evals/preference_reversal.json`](evals/preference_reversal.json) — newer preference superseding stale preference.
- [`evals/fact_correction.json`](evals/fact_correction.json) — corrected fact superseding stale fact.
- [`evals/counterparty_boundary.json`](evals/counterparty_boundary.json) — preferences isolated by counterparty.
- [`evals/travel_logistics.json`](evals/travel_logistics.json) — durable travel logistics and preferences.
- [`evals/expiring_discount.json`](evals/expiring_discount.json) — expired time-sensitive facts not reused.
- [`evals/noisy_style_preference.json`](evals/noisy_style_preference.json) — longitudinal pressure: style preferences must survive injected noise and a simulated follow-up, measured at two checkpoints for trajectory.

Included starter profiles:

- [`evals/no_maintenance_profile.json`](evals/no_maintenance_profile.json) — Formation plus Recall without scheduled maintenance.
- [`evals/default_profile.json`](evals/default_profile.json) — daydream maintenance every two normal turns.
- [`evals/quick_profile.json`](evals/quick_profile.json) — quick maintenance every two normal turns.
- [`evals/llm_judge_profile.json`](evals/llm_judge_profile.json) — LLM judge with three recall samples per checkpoint for mean±stddev scoring.

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

Automatic on first open, and resumable. The 1.x rows are staged verbatim into
`kip_legacy_v1`, the colliding collection names are cleared, and each row becomes
a 2.0 element: a Concept stays a Concept, and a fact-like Proposition becomes a
truth-neutral Proposition plus a positive Assertion — without one, nothing would
be believed after the migration, because silence in 2.0 means *insufficient*
rather than assent.

Three things to know before you start it:

- **Back up the object store first.** The staging is copy-then-drop, so nothing
  is read into memory and destroyed — but the migration does drop the 1.x
  `concepts` and `propositions` collections in place, and it is **one-way**: a
  KIP 1.x build reopening the same store afterwards will not find its
  collections. The original rows survive in `kip_legacy_v1` and stay
  inspectable, which is not the same as being able to roll back.
- **It runs per space, on first access — not at startup.** Spaces are
  lazy-loaded, so the service comes up before anything has migrated, and the
  first request that touches a space is what pays for it and where a failure
  surfaces. A large space makes that one request slow; a crash mid-way resumes
  on the next attempt rather than starting over.
- **Migrate one space at a time if you can.** Nothing serialises them, and each
  is independent, so a problem found on a small space is a problem you have not
  yet had on the rest.

Migrated claims carry `mode: "imported"`, and 1.x `(type, name)` identity becomes
a 2.0 `key`, so counterparty lookups keep resolving. Types the Cognitive Memory
Profile also declares are adopted onto it; a type only this deployment used keeps
a generated `kip://legacy/nexus@1.0.0` symbol.

The usage ledger does not survive: it is keyed by element id and 2.0 re-mints
those, so recall counters, correction history and self-test coverage restart
empty. The memories themselves are unaffected.

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

```toml
# Embedding the library: memory only
anda_brain = "0.11"

# …or the full surface
anda_brain = { version = "0.11", features = ["mcp", "wiki"] }
```

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
