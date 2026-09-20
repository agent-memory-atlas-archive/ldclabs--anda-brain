# Memory utility

**[English](UTILITY_RUNTIME.md) | [中文](UTILITY_RUNTIME_cn.md)**

The utility runtime records memory delivery and independently verified use, then optionally
calibrates **Concept `MnemonicState.utility`**. Retrieval counts, citations,
likes, model self-reports and task success alone never increase utility.
Assertion confidence, BELIEF, Skill standing and execution authority are unchanged.

## Delivery receipts

Every configured Space installs an off-graph `RecallReceipts` store. Bounded Recall
retains the exact packet digest, delivered item digests, actual native references
and versions, semantic content pins, coverage, native basis and effective budget.
The receipt always retains `semantic_complete:false` and `action_ready:false`.
Receipt writes are owned and read back; Recall does not mutate the cognitive graph.

Structured Recall returns an optional `recall_receipt:{id,digest,scope}` handle.
Plain/direct agent calls retain the normal `conversation` identifier;
trusted hosts resolve its handle through
`Space::recall_receipts().for_conversation(id)`. A natural-language legacy answer
has `delivery:"legacy_trace_only"`: actual retrieved versions remain diagnostic,
but prose cannot prove which complete items were delivered. Such receipts are not
eligible for automatic contribution credit. A null/insufficient packet also cannot
authorize attribution. No packet content is added through receipt metadata.
The receipt's `scope.space_instance` identifies the delivery journal; OutcomeInput
still uses the actual action/learning instance, not the delivery-journal instance.

Action gates issue a receipt for their actual bounded context. A trusted policy can
instead set `ContextRequest.recall_receipt` to a previous bounded Recall handle.
Every used/applied reference must then match its delivered semantic content and
remain currently readable. The native Decision's rationale retains JSON containing
the exact handle; `used_refs` and `applied_revisions` remain native fields. A
clarification child issues its own fresh receipt for the parent question.

## Attribution

The runtime follows **Outcome → Attempt → Decision → used/applied references**,
verifies the actual creation transaction's Principal and resolves current records.
Semantic attribution fields cannot impersonate the observer. Retrieved-only items
receive no credit. More than one jointly used memory produces an
`inseparable_bundle` no-update receipt; the result is never divided or copied into
individual scores. Non-Concept targets retain audit receipts without a MnemonicState
write or synthetic Experience/Insight duplication.

Two compiled methods are supported:

| Method | Evidence |
| --- | --- |
| `single_contribution_v1` | Exactly one used memory; a qualified independent observer's bounded `ContributionWitness`, matching the actual Attempt, Decision, delivery receipt and memory pin |
| `paired_revision_v1` | The configured learning controller's frozen, revision-bound native Trial/Evaluation and complete replay evidence; one candidate revision is the only changed memory, with the same factual memory/model/tools/budget within the trial |

`paired_revision_v1` requires `learning` and supports the
`tool_workflow.precondition.v1` contract. It reuses the native verdict and current
evidence validation, not a second evaluator or a guessed baseline. Unknown/missing
control evidence remains insufficient. Old revision credit is not transferred to a
new `current_revision`; revoked or dependency-unverified procedures are not ranked.

### Independent witness

An already authorized observer submits the normal `POST /v1/{space_id}/outcomes`
request with either `utility:{witness:ContributionWitness}` or
`utility:{witness_ref:"E-..."}`. The former retains the witness in the signed
native Outcome Evidence payload, so the observer need not write KIP. The latter
references native Evidence authored by the same independent Principal, created
after the Attempt and before the Outcome. Exactly one form is accepted for credit.
No new model/MCP writer or observer registration tool is exposed.

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

Effect and bounds are normalized to `[-1,1]`, with
`lower_bound ≤ effect ≤ upper_bound`; confidence is strictly below 1. The signed
instrument contract defines `sampling_unit` from an actual independent business
event/root. Repeated native copies or Outcomes with that unit cannot earn another
step, even under another target. The operator must establish the instrument's
physical independence and causal measurement quality; a signed assertion is not
an empirical guarantee. Test witnesses are mechanism fixtures only.

## Configuration

Runtime startup configuration accepts `spaces.<id>.utility: UtilityConfig`. See the
disabled [utility.runtime.example.json](utility.runtime.example.json) template.
Trusted Rust hosts can populate `SpaceRuntimeBindings.utility`. Methods have
explicit versions and bind observer identity/configuration/domain, family, metric,
window, environment and tool versions. Scope pins must describe the actual business
execution environment, including the model/context assumptions relevant to that
instrument. Different scopes are not pooled.

Parameters have **no empirical defaults**: `step_cap`,
`minimum_independent_samples`, `gain`, `minimum_confidence` and optional
`initial_utility` must be declared. `calibration` binds the exact digest from
`UtilityConfig::contract_digest()`, reviewer, approval and reviewed material.
`apply:true` is rejected without both parameters and approval. Without them the
runtime records proposals/no-update reasons. `automatic`, `apply` and `rank` are
separate switches; an isolated host's `automatic=false` prevents background work.

The first single-contribution method selects the first fixed `minimum_independent_samples`
unique unconsumed units in stable native-reference order. It uses the mean effect,
the weighted interval envelope and a conservative union-bound confidence. Paired
mode consumes one whole native comparison at a time, using its fixed cohort and a
two-sided Hoeffding confidence for the chosen effect direction. Threshold changes
require a new matching calibration; there is no favorable-window filtering.

For an eligible group:

```text
delta = clip(gain × mean_effect, -step_cap, step_cap)
u_new = clamp(u_old + delta, 0, 1)
```

The receipt retains old/new values, delta, parameters, method/configuration digests,
selected/excluded outcomes, native evidence, uncertainty, independent sample count
and prior receipt. A starting value without previous calibration remains explicitly
an admission assumption. Missing, unknown, contested, invalidated, inseparable or
insufficient evidence preserves the current utility with a reason.

## Persistence and ranking

The consumer stores its complete intent before writing. Native Evidence receipt,
calibration Activity, Concept utility and native idempotency key commit in one
`MUTATE` transaction. Native CAS-guarded no-ops assert input versions without
changing their fields or versions. Derived memories retain the exact existing
DependencyBasis in a separate same-transaction metadata-producing Activity;
calibration does not invent new dependency approval or change authority.

Checkpoint loss replays the same native transaction before marking units consumed.
CAS conflicts require a fresh read and a new attempt generation, with no repeated
effect. Late/correction audit receipts stay separate from the original Outcome.
An authenticated correction suspends ranking immediately and produces a new
no-update/recomputation receipt; it never rewrites history or silently subtracts
an unvalidated amount. Storage/read failures remain explicit and retain retry work.

The attention scheduler starts an independent owned utility pass: up to eight observation-index slots
and one target per pass. Numeric catalog slots survive restart. Limits are 4096
targets, 64 pending samples per target, 512 native input guards per transaction,
and 8 MiB per private journal object; exceeding a bound reports backpressure.
The observation index retains its existing one-million-snapshot limit. Used
samples leave the hot set; permanent unit claims and native history remain.

Budgeted Recall optionally orders verified utility **only within the existing
priority**. Required constraints and warnings remain an indivisible set; procedure
checks and native uncertainty stay mandatory. Ranking is bounded to an optional
500 ms read window and falls back to normal order if unavailable. Raw utility
without a current verified calibration receipt is ignored. Packets still grant
neither semantic completeness nor execution permission.

Runtime status adds `utility` configuration/calibration/automatic/apply/ranking
flags and a recovery reason, without exposing private calibration material.
Audit/application methods remain trusted Rust APIs: `enqueue`, `evaluate`, `rank`
and `status` on `Space::utility()`. There is no model-facing score setter.

Utility mechanism tests are not evidence of empirical memory improvement. Production
method parameters and observer quality still need independently reviewed results;
MIB retains the separate outcome/improvement release gate.
