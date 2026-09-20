# Contextual trust runtime

**[English](TRUST_RUNTIME.md) | [中文](TRUST_RUNTIME_cn.md)**

The trust runtime produces governed source-calibration proposals and optionally applies them through
Nexus 0.13.1's atomic trust/proposal/audit API. It is available in the lean library.
Nothing is enabled by default; startup configuration never grants `manage_trust`.
This is a source-weight policy, not Assertion confidence, factual truth, utility,
Skill standing, or execution authority. The legacy `source_reliability` correction
counts are still diagnostic and are not calibration inputs.

## Supported method

`binary-fact-accuracy-v1` uses an independently verified binary verdict about an
actual source Assertion in one exact predicate and one exact context. Both selectors
are mandatory; the Assertion must carry exactly that context. Its semantic actor
must be a canonical Concept, and its actual native writer must differ from the
registered verifier. Hypothetical/predicted/uncertain commitments, raw Propositions,
other predicates/contexts and assessments outside the claim's applicability interval
are refused. Historical supersession does not rewrite what the source once claimed.

The registered instrument must validate source attribution, factual accuracy, stable
environment and timing, and retain the underlying comparison in `material`.
`verified_fact` plus an explicit `correct` boolean supplies a sample. Execution
failure, missing preconditions, environmental change and unknown causes produce
exclusions, regardless of a boolean or action outcome score supplied alongside them.
The host does not interpret Assertion confidence as a probability. Predictive
probability calibration is not implemented by this method.

Roots are instrument-defined correlation groups. Copies and summaries keep the same
`root_key`. The runtime also deduplicates the source's logical commitment
(actor/Proposition/stance/context/valid-time), so new event IDs, new verification
Evidence IDs, or relabeled roots cannot make repeated checks of one fact independent.
A root/commitment already consumed by an applied calibration is not another sample
for a later trust step. Corrected replacement evidence is not a fresh independent
fact merely because it has a new ID.

With explicit parameters, the host selects the first `minimum_samples` eligible,
unique, unconsumed samples in native creation-sequence order. For binary mean
accuracy `a` and `n` independent samples, it reports a two-sided Hoeffding interval
with radius `sqrt(ln(2/alpha)/(2*n))`, clipped to `[0,1]`, and confidence `1-alpha`.
The proposed weight is `clamp(old + clip(gain*(a-old), -step_cap, step_cap), 0, 1)`.
The old value comes from the actual native lookup, including existing scoped rules;
ambiguous lookup is an error. There are no default statistical parameters. Missing
parameters, insufficient samples or unresolved intake/material yield no applicable
proposal. Missing observations are never guessed as success or failure.

The independence and sampling assumptions require deployment validation. A signature,
configuration digest or reviewed-material field is not proof of physical independence
or empirical accuracy. Validate the instrument, population, parameters and error
rates independently in MIB before enabling automatic changes.

## Configuration and authority

Install the per-Space `trust` field through `BRAIN_RUNTIME_CONFIG` or trusted
`SpaceRuntimeBindings`, before loading the Space. The
[template](trust.runtime.example.json) has null parameters/calibration and all
switches off. Replace its Space ID, canonical context reference (`C-1` is only a
placeholder), exact predicate URI, instrument/environment digests and service IDs.
No model tool, ordinary HTTP request or MCP tool can install or change this binding.

| Setting | Meaning |
| --- | --- |
| `proposer_principal` | Trusted reader/deriver that stores reviewable artifacts; separate from verifier/governor/channel callers |
| `observer` | Registered native verifier principal, configuration digest and independent control domain |
| `governor_principal` | Explicit governance service identity; never inferred from a semantic actor |
| `parameters` | `minimum_samples` 1–32, `alpha` in `(0,1)`, `gain` and `step_cap` in `(0,1]`; no defaults |
| `calibration` | Bounded nonempty review material, reviewer distinct from proposer/verifier, exact `contract_digest`, and explicit approval |
| `automatic` | Admit bounded background discovery/proposal work |
| `apply` | Allow explicitly reviewed applications; requires calibrated parameters and a named governor |
| `automatic_apply` | Also allow the registered approved method to supply the review policy automatically; requires both other switches |

`bootstrap:true` can provision proposer read/read_history/read_governance_history/
create/derive and verifier read/read_history/create/derive/record_outcome permissions.
It does not provision the governor or `manage_trust`. An existing governance
administrator must separately provision the governor's current `manage_trust` and
necessary material reads. The governor does not need ordinary Create merely to
record the native governance audit. Restart does not restore revoked grants.
Automatic use constructs only the explicitly installed host service identity;
ordinary observation bodies cannot supply an identity or approval.

## Host APIs and outcome discovery

`Space::trust()` returns the trusted runtime. These are Rust host APIs, not new
HTTP/MCP mutation endpoints:

| API | Behavior |
| --- | --- |
| `record_verification(auth, TrustVerificationInput)` | Fresh authenticated verifier intake; returns native Evidence ID |
| `enqueue(evidence_ref)` | Revalidate an already committed native verification for this contract |
| `propose(actor_ref)` | Explicitly prepare/review a new frozen proposal, with exclusions and uncertainty |
| `proposals(after, limit)` | Bounded current proposal inventory, limit 1–32 |
| `proposal(id)` / `receipt(id)` | Governed retained proposal / recorded application receipt |
| `apply(auth, id, reason)` | Current configured governor + ManageTrust, exact proposal and native CAS |
| `abandon(auth, id, reason)` | Explicitly close a provably uncommitted review; an applied change cannot be abandoned |
| `propose_restore(auth, previous_id, reason, evidence_refs)` | Prepare an exact scoped restoration for separate application |
| `status()` | Configuration, switches, calibration, governor authority and recovery reason |

`TrustVerificationInput` contains `event_key`, `root_key`, `assertion_ref`,
`assessed_at`, `cause`, optional `correct`, and nonempty object `material`. The
complete input is capped at 16 KiB. Calls must use an AuthContext from a trusted
authentication boundary, never one reconstructed from saved JSON or `asserted_by`.
An identical event retry reuses its native identity; a conflicting body is rejected.
An interrupted intake without committed material requires the verifier to retry the
same input with fresh authentication. Other facts do not turn that unknown into a
complete sample set.

A registered outcome observer can announce an existing verification in a normal measurement
outcome's `payload: {"trust_verification_ref":"E-…"}`. The outcome remains bound to
its real Attempt/Decision and normal outcome authentication. The trust consumer treats this
as a discovery hint only, re-reads the separately authored verification and ignores
operational success/failure/magnitude as reliability scores. Correcting a delivery
outcome does not itself declare that an independently verified fact was wrong; correct
the actual factual Evidence through the native correction protocol when it is wrong.

## Atomicity, recovery and limits

The runtime preserves global actor weights, the global default and every neighboring
rule. A proposal edits only the registered actor/predicate/context selector. Native
`apply_trust_calibration` commits the reviewed proposal/method association, trust
control version and Governance audit in the same redo transaction. Source Evidence
is immutable, and its active/corrected state is checked again inside that transaction.
The model and semantic actor do not acquire any control-plane permission.

Before native writes, private CAS records retain the intake locator or reviewed
application intent. They hold only refs, hashes, counters and status. Verification
text lives in native Artifacts bound to the verification Evidence itself and every
source Claim/Proposition/actor/context; source or verification purge also revokes that
material. Proposal/review artifacts inherit their material-source restrictions.
Cancelling an API waiter does not cancel admitted writes; close drains them.

A lost ACK is reconciled against the exact historical native trust version and its
proposal/author. Historical lookup uses at most 64 public sequence reads, not a
private idempotency-key encoding or an unbounded host scan. Later settings or a later
Evidence correction do not turn an already committed change into an uncommitted one.
A conflicting native version never silently rebases a proposal or overwrites another
setting. Request a fresh explicit proposal/review, or abandon a provably uncommitted
review. Restoration creates a new version and reason, requires current Evidence,
restores only the original scoped rule (or its absence), and refuses to erase a
subsequently different scoped setting. It does not claim new independent samples.

Native trust-version changes invalidate old ProjectionBasis, Watch and dependency
checks. Brain also clears its diagnostic miss cache and marks registered attention
work dirty. It never auto-rearms a Watch to hide an observation gap. An applied
control remains governance history: later data disputes require a qualified new
review/restoration, not a rewrite or automatic erasure of the old setting.

The private catalog admits at most 4,096 targets and 64 pending verification records
per target. Proposal material is capped at 256 KiB / 256 source refs; native selected
Evidence is at most 32 records. The reused private CAS journal has an 8 MiB object cap.
A background pass reads at most eight observation index slots and handles one target, separately
from Watch/formation/maintenance work. One pass runs per Space, and one live owner
process is required per storage shard. Storage failures keep the cursor/intent;
known out-of-contract announcements are retained as exclusions. Runtime HTTP/MCP
status adds `trust` switches and a recovery reason, not raw samples or a management tool.
