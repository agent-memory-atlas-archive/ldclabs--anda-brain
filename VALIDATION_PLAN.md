# Runtime observability and empirical validation plan

**[English](VALIDATION_PLAN.md) | [中文](VALIDATION_PLAN_cn.md)**

Status: proposed implementation and acceptance work following **0.12.1**, updated 2026-09-23.
This document specifies work still to do; it does not announce implemented metrics,
completed business calibration or permission to enable automatic learning.
KIP v2 has not been deployed. Start with fresh v2 Spaces; old-data inventory and
migration are out of scope. Normal restart, eviction, cancellation, revocation and
lost-acknowledgement recovery remain required.

## 1. Baseline and ownership

| Owner | Reuse | Remaining work |
| --- | --- | --- |
| Brain | [attention reports](anda_brain/src/attention/mod.rs), [action receipts](anda_brain/src/action/service.rs), [outcomes](anda_brain/src/consequence/service.rs), [runtime status](anda_brain/src/runtime_api/service.rs) | Durable metric identities, bounded census, stage timing, cost attribution and authorized export |
| Brain | [workflow HTTP adapter](anda_brain/src/learning/workflow_http.rs), [Recall receipts](anda_brain/src/recall_receipt.rs), utility/semantic/trust runtimes | Expose trusted measurement/audit seams needed by the evaluator; preserve native authority and budgets |
| MIB | [longitudinal harness](https://github.com/ldclabs/MIB/blob/main/docs/harness/MIB-Learning-Longitudinal.md), product regressions, lock/replay/report verification | Real-host admission, complete attempt-level measurement, semantic/utility/trust experiment profiles and verifiable reports |
| Business host / Anda Bot integration | Isolated runs and existing Agent/memory backend protocols | Actual executor, independent observer, plan factory and provider-usage bindings; accurately describe supported conditions |
| Deployment operator / independent reviewer | Existing versioned runtime configuration | Freeze contracts, approve measured calibration and enable each capability separately |

Checked local MIB entry points: `scripts/check-brain-product-regression.py`,
`src/mib_runner/learning/benchmark.py`, and `docs/harness/MIB-Learning-Longitudinal.md`.
The current product script uses a fixture, not Brain. The longitudinal report
currently cannot establish complete native learning proof; ordinary Agent output
lacks complete attempt-level provider usage. Fix those gaps before claiming a PASS.
No new AndaDB release is assumed. Verify public native receipt/history APIs first;
if a necessary field is unavailable, isolate that upstream prerequisite and publish
it before consuming it. Do not add sibling path overrides or an offline evaluator
inside Brain.

## 2. Freeze the measurement contract

Add `anda_brain/src/observability/` with a versioned typed event, a bounded store,
aggregation and an optional host-installed exporter. These are **new proposed**
modules. Wire them through `lib.rs`, `runtime_api/config.rs`, `space.rs` and
`space/attention.rs`; registration must precede event-producing work.

A `RuntimeMeasurement` contains format, shard, Space instance, native sequence,
stage, operation ID, parent/correlation references, event time, observation time,
clock domain, outcome/reason code, replay identity and optional usage. Persist no
prompt, answer, raw Evidence, user identifier, endpoint credential or bearer token.
Detailed correlation references are available only to trusted host auditors; metric
labels use bounded stage/status/reason enums, not Space/Watch/actor/event IDs.

Derive a stable event ID from instance + stage + native operation identity + native
receipt version. Observing/replaying a native completion does not create a second
logical completion. Actual network/model retries have distinct call-attempt IDs
and contribute their real cost. Keep logical completion and call attempts separate.
Do not copy private Nexus idempotency formats into the exporter.

| Measurement | Definition and authoritative source |
| --- | --- |
| Deadline → wake | For timed Watches, `max(0, native persisted fire time - normalized due time)`; early legitimate matches are not clock errors; delta Watches without due time use trigger-commit → wake as a separate series |
| Wake → gate | Native gate commit time minus the consumed wake's creation time; keep each clarification/continuation lineage distinct |
| Gate → dispatch | Native first dispatch admission minus gate commit; report target acceptance and terminal independent Outcome as additional stages |
| Pending / oldest age | Nonterminal work at a fixed census boundary, by bounded reason; unknown dispatch stays pending and cancelled/completed work is excluded |
| Coverage lag | Frozen target sequence minus proven consumed sequence within the same native Space/arm; time lag only when trusted sequence timestamps are available |
| Retry / dedup / rejection | Distinct attempted operations and native replay acknowledgements; rejected observations are metadata only, never trusted graph facts |
| Outcome consumption | Qualified receipt indices not yet acknowledged by each configured consumer; exclude permanent exclusions and distinguish incomplete intake |
| Learning / review | Hot and archived counts, capacity, outstanding review obligations and overdue age; archive does not imply adoption |
| Cost | Every actual model/tool attempt, including formation, recall, maintenance, semantic evaluation, gate, executor and observer; distinguish tokens, tool calls, elapsed time and currency |

Use monotonic clocks for a process-local duration. Across restarts/services, compare
only validated timestamps in the same clock domain, retaining skew/unknown flags.
Never mix experimental business time and wall time. Missing or negative-clock
measurements are unknown, not zero. Record `measurement_started_at`; no historical
backfill is required for this proposed observability rollout.

Each cost entry has request/attempt identity, scope, stage, source, token/call counts,
nullable amount/currency and `accounting_complete`. Preserve cumulative provider
snapshots separately; do not sum them as deltas. Monetary conversion requires a
pinned provider/model price sheet, currency and effective time. Unknown billing or
missing failed-call usage prevents a complete total. Estimated tokenizer counts and
provider billing remain separate fields.

## 3. Implement recoverable collection and bounded export

1. Add hooks in `attention/service.rs`, `attention/semantic/runtime.rs`,
   `action/{gate,native,dispatch}.rs`, `consequence/service.rs`,
   `learning/runtime/{scheduler,catalog,settlement}.rs`, and the utility/trust
   transaction/application paths. Reconcile native results before recording a
   committed metric. Reuse returned native refs and current authorized reads.
2. Before work starts, persist a bounded measurement-discovery intent. Native and
   telemetry writes are separate transactions. Lost telemetry ACKs recover via the
   same event ID; native commit followed by process loss is repaired by a bounded
   receipt census. Pending intent is not a fabricated completion.
3. Default proposed limits: 16 admission slots, 200 census records per pass,
   64 KiB per event, 10,000 queued records or 64 MiB per shard, whichever fills
   first. Evict exported telemetry after seven days; expose limits and retention
   as explicit host settings. Store sequence checkpoints with CAS. Report gaps,
   queue saturation, dropped optional samples and retention boundaries.
4. Telemetry failure must not alter a Decision, Outcome, trust version or permission,
   or stall already admitted native writes. Mark telemetry incomplete and continue
   bounded reconciliation; if its source is erased/unavailable, retain an explicit
   missing measurement. Never retain source text to make metrics recoverable.
5. A census pins a scan boundary and persists its cursor. Partial counts are lower
   bounds with `complete:false`, `as_of`, `scanned` and `next_cursor`. Do not present
   the first page as a complete backlog. Use generation/revision guards to avoid
   mixing old and new work; clocks and scan lag remain visible.
6. Add optional `RuntimeStatus.observability` for configuration, health and
   completeness. Tenant-wide census/timings are auditor-only; ordinary recipients
   retain existing filtered counts. Export shard aggregates through a trusted Rust
   sink first; any deployment metrics endpoint must be separately authenticated.
   No model-facing writer or implicit public metrics endpoint.
7. Bound histogram storage with versioned fixed buckets (planned seconds:
   0.01, 0.05, 0.1, 0.5, 1, 5, 15, 30, 60, 120, 300, 900, overflow).
   Reports identify bucket bounds and incomplete windows. Use exact retained
   fixture durations for regression percentiles; bucket estimates cannot silently
   become exact P95. Export absolute window aggregates with window/event identity
   so an exporter restart cannot double-increment a remote counter.

Tests live near `observability/` and `space/tests/`: same commit observed twice,
ACK loss, crash between commits, native failure, stale census, multiple pages,
clock rollback, retained unknown dispatch, cost missingness, purge/revocation,
queue exhaustion, cancellation/close drain and reader/auditor isolation.

Acceptance: deterministic fixtures reconstruct exactly one logical completion per
native receipt and every real call attempt, without score or permission changes.
Run the existing 20-Space/200-Watch, five-second-tick benchmark with metrics off/on:
P95 deadline→wake stays ≤60 s, and instrumentation adds ≤5% throughput/latency
regression under the same recorded host/storage/load. These are proposed engineering
acceptance targets, not a production SLA. Report overload and downtime separately.

## 4. Connect MIB to the actual host

Work in MIB and the business host as separate changes after the Brain measurement
contract is stable. Reuse `learning/benchmark.py`, its immutable locks and score
verification. Add real executor/observer/source bindings for
`tool_workflow.precondition.v1`; only then admit `normal` and isolated `ungated`.
A `persistent` host with `learning:false` must still be refused. `ungated` bypasses
recommendation standing only inside an isolated native trial; execution authority,
fences, budgets and independent observation remain enforced in every condition.

Extend MIB's report/schema/verification to associate each decision, attempt, outcome,
trial, evaluation and provider call with its exact run/revision. Verify actual tool
journals and measured usage, not stored success labels or descriptor declarations.
Missing accounting preserves unknown/insufficient. Keep training, validation,
world seeds, oracle labels and evaluator audit material out of business prompts,
Formation and candidate generation. Immutable factual baselines can be cloned before
installing live runtime identities; never fork configured attention/learning journals.

Add dedicated profiles and validators under MIB's `profiles/`, `schemas/`,
`src/mib_runner/` and `tests/`; proposed semantic/utility/trust commands must be
implemented and documented there before being used. Do not imply that the existing
longitudinal command already evaluates those three capabilities.

## 5. Preregister independent empirical gates

Before validation, lock code commits, profile/contract digests, model deployment and
prompt, tokenizer, tools, environment, independent observer identities, factual
baseline, train/calibration/validation split, all planned seeds/conditions, budgets,
missingness rules, sample-size calculation and stopping rules. Tune on training and
calibration only; freeze parameters before held-out evaluation. Changing a material
pin creates a new calibration and requires a new held-out run.

The following are **initial proposed release criteria**, to be approved and frozen
before seeing validation results; they are not hidden production defaults:

| Capability | Comparison | Required result |
| --- | --- | --- |
| Learning | Same business model/tools/budget: normal, no_memory, isolated ungated; early success, negative transfer, drift and revocation strata | Paired lower confidence bound of normal minus no_memory success ≥5 percentage points; unsafe-rate increase upper bound ≤1 point; no use of revoked revisions; all strata reported |
| Semantic Watch | Blinded independently labelled event pages, text/mixed/empty/unknown cases, pinned model/prompt | False-silence rate upper bound ≤1%; delta precision/recall and determinate-page completion lower bounds ≥95%; no unknown/missing judgment advances coverage; report abstention and latency |
| Utility | Same-priority Recall ranking on/off; single-contribution and paired-revision methods separately | Paired task-success lower-bound improvement >0; unsafe-rate increase upper bound ≤1 point; no score for retrieved-only/bundled/replayed evidence and no loss of mandatory constraints |
| Contextual trust | Frozen native trust vs scoped calibrated weights, on independent held-out factual roots | Held-out scoped BELIEF decision-error reduction lower bound >0; unsupported accepted-belief rate increase upper bound ≤1 point; no global/other-scope effect or confidence mutation; restoration and drift tested |
| Accounting / authority | Every actual provider/tool attempt and exact native provenance | Complete measured usage for budget-qualified native success; no fabricated zero cost, authority bypass, credential disclosure or duplicate external effect |

For semantic tests, the false-silence denominator is all labelled Watch/deadline
windows that contain a qualifying match, not only windows where the model emitted
silence. Delta precision uses emitted positives; recall uses all labelled positives.
Completion uses independently labelled determinate pages; expected-unknown cases
are a separate stratum and must not advance coverage. An empty denominator is
insufficient, not perfect performance. Report every planned window and refusal.

Use family-wise alpha 0.05 across the preregistered efficacy/safety gates, with the
correction rule and primary hypotheses frozen; target power ≥80% for the declared
minimum effect. Estimate variance/cluster size on a disjoint pilot and lock the
resulting sample size. There is no universal sample count for all deployments.
Resample/compare independent task/root clusters, not repeated events or Evidence
copies. Plan denominators include failures, timeouts and missing runs; do not score
missing runs as successes or drop inconvenient strata. A critical authority/safety
violation fails immediately. Insufficient evidence does not pass.

For illustration, zero errors in 299 independent cases gives a one-sided 95% upper
bound below 1% before multiplicity correction. With corrected tail probability `a`,
use at least `ceil(log(a)/log(0.99))` zero-error independent cases for that bound;
clusters and observed errors require the preregistered interval calculation. Native
trust's maximum 32 samples per proposal and utility's native group limits remain
unchanged. Larger deployment validation uses separate independent experiments;
it does not override runtime bounds or reuse consumed roots.

Trust's current method is binary factual accuracy, not forecast-probability
calibration. Label accepted/rejected/insufficient BELIEF separately and report
coverage/abstention; a method cannot improve error simply by suppressing every
answer. Lock an acceptable coverage-loss margin of at most 1 percentage point for
trust and utility comparisons. Do not use Brier scores on Assertion confidence.

## 6. Execution order and deliverables

| Order | Owner and deliverable | Exit condition |
| --- | --- | --- |
| 1 | Brain: event/clock/cost schema, configuration and fault tests | Stable identities, bounded storage, explicit unknown and privacy rules |
| 2 | Brain: hooks, census, status and host exporter | Recovery/privacy/load tests above pass; bilingual API/guide/skill updated |
| 3 | Business host + MIB: actual bindings and attempt-level accounting | No-model integration proves real native provenance and refuses incomplete capabilities |
| 4 | MIB: four frozen validation profiles and replay validators | Disjoint pilot determines sample sizes; lock includes every criterion; engineering reports remain not_evaluated |
| 5 | Independent evaluator: real held-out runs | Reproducible report/lock/progress/raw governed artifacts; each gate pass/fail/insufficient with exclusions and uncertainty |
| 6 | Operator + reviewer: calibration import and limited rollout | Exact configuration digest approved; independent feature switches and recovery drill verified |

Existing commands (run from the indicated repository; provider runs require an
explicit cost budget and real configured endpoints):

```sh
# Brain engineering checks
cargo fmt --check
cargo clippy -p anda_brain --all-targets --all-features -- -D warnings
RUST_MIN_STACK=16777216 cargo test -p anda_brain --all-features
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features wiki
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features mcp

# In MIB: fixture/protocol verification, not real Brain quality
uv sync --extra test
uv run python scripts/check-brain-product-regression.py --output-dir /private/tmp/brain-product-check
uv run pytest tests/test_product_regression.py

# In MIB: after real bindings, accounting and report verification are implemented.
# Use a reviewed copy of examples/learning-longitudinal/experiment.json with real endpoints.
uv run python -m mib_runner learning-benchmark /absolute/private/experiment.json --output /absolute/private/run.report.json
uv run python -m mib_runner verify-score /absolute/private/run.report.json
```

Create a new evaluator-private output path for each run. `--resume-lock` retains
frozen tasks but starts fresh isolated runs; it is not permission to replay an
uncertain old external action. `verify-score` proves report consistency, not all
business acceptance gates; the new profile validators must check those gates too.

Calibration deliverables map to existing `LearningCalibration`, `UtilityCalibration`
and `TrustCalibration` with exact runtime digests, distinct reviewer and replayable
material references. Semantic validation pins the exact evaluator configuration.
Roll out read-only telemetry first, then proposals with automatic application off,
then one qualified task/scope. Proposed canary: seven days **and** the locked minimum
independent exposure count, whichever takes longer. Stop new automation on authority
violations, broken measurement or drift; reconcile unknown dispatches, drain writes
and retain native history. Trust rollback is a new governed restoration; never
rewrite assertions or delete governance evidence to make a rollout appear healthy.
