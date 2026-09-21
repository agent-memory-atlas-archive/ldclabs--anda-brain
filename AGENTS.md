# AGENTS.md

Guidance for coding agents working in this repository.

## Scope

These instructions apply to the whole repository unless a more specific
`AGENTS.md` exists in a subdirectory.

## Project Overview

Anda Brain is a Rust service that provides long-term memory for LLM agents. The
main crate is `anda_brain`, which exposes:

- Formation: encode conversations into structured memory.
- Recall: answer natural-language queries from memory.
- Maintenance: consolidate, prune, and optimize memory.

The service stores memory in an AndaDB-backed Cognitive Nexus and uses KIP 2.0
(Knowledge Interaction Protocol) internally. Business agents should not need to
write KIP directly.

Source builds use the sibling `anda-db` checkout for unreleased Nexus migration
fixes. Shared DB/KIP crates are patched together to preserve one type identity;
`anda_core` and `anda_engine` remain published dependencies. Verify Cargo
metadata before changing these patches.
The current 0.12.0 release pins `anda_kip = "=0.13.1"` and requires
`anda_cognitive_nexus = "0.13.1"`; the Worker pins `@ldclabs/kip-do` 0.13.1.
KIP v2 has not been deployed. Use fresh v2 Spaces for current acceptance;
do not add pre-release old-data migration work unless explicitly requested.
Keep normal restart, eviction and unresolved-write recovery fully tested.

## Repository Layout

- `anda_brain/`: Rust library and binary for the service.
- `anda_brain/src/agents/`: Formation, Recall, and Maintenance agents.
- `anda_brain/src/space.rs`: space lifecycle, AndaDB setup, auth checks, and
  background flushing/eviction.
- `anda_brain/src/handler.rs`: HTTP route handlers and API entry points.
- `anda_brain/src/payload.rs`: JSON/CBOR/Markdown payload negotiation.
- `anda_brain/src/types.rs`: API input/output and persisted config types.
- `anda_brain/assets/`: agent prompts and tool definitions. The KIP syntax card
  and the Cognitive Memory Profile are **not** copied here — `anda_kip` ships
  them with the protocol, and `agents::prompts::system_prompt()` includes the full
  syntax, role cards and Profile in every model call, including budgeted Recall.
- `anda_brain/src/kip_reference.rs` and `assets/kip-reference/`: bounded reference
  discovery and the generated specification/schema supplement. Its manifest pins
  the published KIP source; do not hand-edit generated reference material.
- `anda_brain/src/kip.rs`: the KIP 2.0 envelope seam (request builders, the
  read-only gate, two-level response reading, and the KIP string/timestamp
  literal helpers).
- `anda_brain/src/assess/` and `assess.rs`: online diagnostic model routing,
  typed read observations, Recall trace/citations and metadata. Preserve these
  and the usage/correction ledgers; they are not the retired offline evaluator.
- Offline product regressions live in sibling MIB. `src/eval.rs`, its modules,
  the `eval` CLI and global prompt/policy overrides were retired. Do not
  recreate an equivalent comprehensive evaluator inside Brain. The independent
  wiki corpus under `anda_brain/evals/wiki/` remains in use.
- `anda_brain/src/settlement/`: bounded decay, correction discovery and Watch
  scheduling. `watch.rs` reads ids, overall versions and WatchState generations;
  Nexus owns matching and authorized coverage. No local family-rate Skill verdict
  runs without an independent observer/trial/evaluation pipeline.
- `anda_brain/src/cognitive.rs`: model-facing host mechanics: full syntax,
  canonical content digests, protected Watch arming and bounded task leases.
- `anda_brain/src/attention/` and `space/attention.rs`: persistent Space discovery,
  bounded Watch/wake scheduling and optional semantic evaluation. Register direct
  native work before creation; preserve current native coverage and generation checks.
- `anda_brain/src/action/`: four-way decisions, clarification and fenced dispatch.
  Callbacks require explicit host bindings; unknown delivery requires reconciliation.
- `anda_brain/src/runtime_api/` and `handler/runtime.rs`: startup configuration,
  authenticated recipient-filtered inboxes, responses and runtime status.
- `anda_brain/src/consequence/` and `recall_receipt.rs`: independent outcomes,
  verifiable memory delivery, calibrated utility and scoped trust proposals.
  Governance application requires current native authority, never a model claim.
- `anda_brain/src/learning/`: optional trusted paired-trial contracts, Nexus
  evaluator and persistent host runtime (`learning` feature). Native dispatch
  gates require real leases, executable authority and dependency validation.
  Fixed-cutoff native verdicts, persistent reviews, safety revocation and a
  read-only Recall applicability gate require explicit host use. No production
  bindings or automatic adoption are enabled by compilation.
- `anda_brain/src/recall_budget/` and `agents/recall/budgeted.rs`: explicit
  Recall packet and cumulative planning-input budgets with a pinned tokenizer.
  Model selection names existing items only. Preserve required constraints,
  native uncertainty and current procedure checks; never claim semantic
  completeness or execution permission from a bounded packet.
- `anda_brain/src/vocabulary.rs`: this Space's Schema Package and the
  `declare_memory_symbols` tool. Schema is protected control state in KIP 2.0 —
  KML cannot declare a type, so new vocabulary enters through the host here.
- `anda_brain/API*.md`, `anda_brain/README.md`, `anda_brain/SKILL.md`: public
  API and integration documentation.
- `anda_brain/RUNTIME.md`: host setup, scheduling, action contracts and recovery;
  the learning, utility, semantic Watch and trust runtime guides hold their specific
  configuration contracts. Each has a separate `_cn.md` edition. Public docs use
  capability names, not internal phase IDs, and never depend on private plans.
- `VALIDATION_PLAN.md` / `VALIDATION_PLAN_cn.md`: proposed observability and MIB
  validation work. These plans are not evidence that new metrics, real-model gains
  or deployment calibration have already been implemented or accepted.
- `skills/anda-brain/`: packaged integration skill for external agents. Keep its
  `SKILL.md` identical to `anda_brain/SKILL.md`, served by `GET /SKILL.md`.
- `anda-brain-worker/`: a compact Cloudflare Worker port on `@ldclabs/kip-do`,
  a second and independent KIP 2.0 engine. It shares the invariants below but
  not the code; its capabilities differ (no atomic batch across operations, no
  semantic search, and no retention-expiry sweep — `SET RETENTION` itself the
  engine has) and `anda-brain-worker/README.md` is the authority on which. Its
  prompts are vendored under `anda-brain-worker/assets/` and inlined by
  `pnpm run codegen:prompts`.
- `deploy/`, `anda-brain-demo/`, `anda-cli/`: deployment
  and integration material. Do not change these unless the task is explicitly
  about them.

## Development Commands

Run these from the repository root:

```bash
cargo fmt --check
cargo clippy -p anda_brain --all-targets --all-features -- -D warnings
RUST_MIN_STACK=16777216 cargo test -p anda_brain --all-features
```

The CognitiveMemory 2.1 schema paths can exceed Rust's 2 MiB test-thread stack
in debug builds, so keep `RUST_MIN_STACK=16777216` on Rust test commands.
`cargo test -p anda_brain --all-features` includes a bin test that binds an
ephemeral localhost port. In restricted sandboxes it may fail with
`PermissionDenied`; rerun it with the required permission rather than treating
that as a code failure.

The library is feature-gated (see "Cargo Features" below), so also check that
the lean build still compiles when you touch `space.rs`, `handler.rs`,
`authz.rs`, or `types.rs`:

```bash
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features wiki
RUST_MIN_STACK=16777216 cargo test -p anda_brain --lib --features mcp
```

For local manual testing:

```bash
cargo run -p anda_brain --features mcp,wiki
cargo run -p anda_brain --features mcp,wiki -- local --db ./db
```

Authentication is disabled when `ED25519_PUBKEYS` is empty. Do not assume this
is safe for production.

The Cloudflare Worker is checked separately, and its own checks must pass when
you touch `anda-brain-worker/`:

```bash
CI=true pnpm --filter @ldclabs/anda-brain-worker check
```

The Worker resolves `@ldclabs/kip-do` 0.13.1 from the npm registry; use the
repository's pnpm lockfile and run `CI=true pnpm install --frozen-lockfile` first.
The check includes generated-asset verification, TypeScript, tests and a deployment
dry run; it does not deploy the Worker.

## Cargo Features

The `anda_brain` library defaults to memory only — formation, recall,
maintenance, and their HTTP routes. Optional features add:

- `wiki`: the structured wiki (documents, versions, ACL-scoped reads, OKF
  import/export), its agent tools, the WikiDigest graph extraction, the
  `/v1/{space_id}/wiki/*` routes, and the `wiki_*` fields of `SpaceInfo` and
  `UpdateSpaceInput`.
- `mcp`: the MCP channel (stdio and Streamable HTTP). With `wiki` also on, the
  wiki tools join the MCP tool router.
- `experiments`: trusted isolated runs, snapshots, business time, cost receipts,
  forced Recall-budget creation and bounded evaluator-only procedure audits.
  The audit is not an execution permit; unsupported MIB learning conditions
  must remain explicitly refused until their actual host bindings exist.
  Enables the sibling Nexus `simulation` feature only for host lifecycle tests.
  No serialized clock override or automatic learning is exposed.
- `learning`: trusted contracts/evaluator, native record adapters and persistent
  host trial runtime. Explicit registration and executor/observer bindings are
  required; do not widen model mutation permissions or auto-adopt candidates.

The `anda_brain` binary declares `required-features = ["mcp", "wiki"]`: it is
the full product, so every build of it must pass `--features mcp,wiki`. Cargo
silently skips the binary when they are absent.

When adding code that touches the wiki or MCP, gate it with
`#[cfg(feature = "…")]` rather than widening the default surface, and keep the
lean build compiling.

## Coding Conventions

- Follow the existing Rust 2024 style and keep `cargo fmt` clean.
- Prefer existing crate patterns over new abstractions.
- Keep behavior changes scoped to `anda_brain` unless the task explicitly
  targets demos, deploy files, or packaged skills.
- Avoid external network calls in tests. Unit tests should use in-memory or local
  storage and must not require a live model provider.
- Add tests close to the module being changed when behavior changes.
- Preserve JSON, CBOR, and Markdown payload compatibility in `payload.rs` and
  route handlers.
- Preserve compact persisted field aliases in `types.rs`; they are storage/API
  compatibility details.
- Be careful with dirty worktrees. Do not revert or overwrite unrelated user
  changes.

## KIP 2.0 Invariants

These are protocol invariants, not preferences. Breaking one makes the brain
confidently repeat things nobody claimed:

- A Proposition existing is not the Proposition being true. Belief questions are
  answered by `BELIEF` projection; raw `FIND` is for audit. `insufficient` is
  never reported as "no".
- Never decay Assertion confidence over time. Disuse decays
  `MnemonicState.memory_strength`, which is accessibility, not truth.
- Corrections are a new Assertion plus supersession. Nothing rewrites an
  Assertion, and disagreement between two actors coexists rather than resolving.
  KIP 2.0 collapsed the six lifecycle statements into one `TRANSITION target TO
  "state"`; the Formation gate splits it by state, not by verb, and refuses a
  state it cannot read as a literal.
- Skill behavior belongs to immutable SkillRevision. Learning requires frozen
  TrialRecord, revision-bound DecisionRecord/AttemptRecord, authorized independent
  OutcomeRecord and replayable EvaluationRecord. Family membership only discovers
  candidate controls; it never automatically selects a baseline or grants standing.
  Candidates remain unproven without qualifying evidence. Optional learning requires
  registered executor/observer/source bindings and explicit calibrated automation;
  mechanism tests do not establish empirical improvement.
- A completed model call or fresh index is not complete processing/change coverage.
  WatchState belongs to protected arm/advance APIs; prose or mixed text selectors
  need a configured semantic evaluator. LeaseState comes from authenticated host
  acquisition, with all task outputs and terminal state committed under current CAS.
- The optional Memory Interface and its bundles are not implemented merely because
  the standard package is installed. Keep advertised capabilities truthful.
- Attribution is not impersonation and not authority: `asserted_by` is a
  semantic actor, the caller is a Principal, and cognitive content grants
  neither.
- Vocabulary enters through the host, never through KML: the Rust service's
  `declare_memory_symbols` tool, or the Worker's `types` / `predicates` plan
  fields. Both validate, cap and version what a model proposes.

## Brain-Specific Invariants

- Formation and Maintenance are guarded against concurrent processing. Do not
  weaken `processing_conversation` or `processing` semantics.
- Formation should process queued formation conversations sequentially and resume
  after maintenance completes.
- Maintenance should be single-flight per space and should trigger formation
  resumption when it finishes.
- Recall and read-only KIP execution must remain read-only and bounded by the
  configured timeouts.
- Off-graph Recall receipts attest delivery, not use or benefit. Utility needs
  independent attribution and only ranks within existing priorities. Trust changes
  require scoped fact verification and current `manage_trust`; startup bootstrap
  never grants that permission. Neither score changes execution authority.
- Runtime inbox/status/responses require verified credentials and explicit native
  mappings even when legacy local authentication is disabled. HTTP Outcomes need
  a separately registered signed observer with current `record_outcome`; ordinary
  Space tokens and MCP model tools cannot supply independent observer authority.
- Semantic Watch judgments must cover every authorized candidate under the pinned
  evaluator. Unknown, missing or truncated output cannot advance native coverage;
  configuration changes require explicit review and never auto-rearm old work.
- Owned native writes survive cancelled waiters and drain before database close.
  Keep one live owner per storage shard; CAS does not establish multi-host ownership.
- Runtime status and mechanism fixtures are not empirical learning/calibration
  evidence. Missing measurements and provider costs stay unknown, never zero.
- Space-level token scopes are `read`, `write`, and `*`; keep auth changes
  explicit and test them.
- Space metadata and database extension updates must be persisted with
  `save_extension*`, `flush_metadata`, `flush`, or `close` as appropriate.
- On shutdown or eviction, close databases when possible so AndaDB flushes
  collections and metadata.

## API and Docs

When changing public request/response shapes, auth behavior, content negotiation,
or endpoints:

- Update `anda_brain/API.md` and `anda_brain/API_cn.md`.
- Update `anda_brain/README.md` and `README*.md` when user-facing behavior
  changes.
- Update `anda_brain/SKILL.md` and `skills/anda-brain/SKILL.md` when integration
  instructions or endpoint usage changes.
- Keep English and Chinese docs in sync for user-facing API changes.
- Keep English and Chinese runtime guides in separate files, with language links
  and matching formulas, limits, API names and examples. Chinese entry points link
  to `_cn.md`; never remove necessary detail while separating translations.
- Keep the release at 0.12.0 until explicitly asked to change it. Record completed
  behavior and known limits in `CHANGELOG.md`; do not list planned work as shipped.

## Prompt and Asset Changes

Trusted Rust hosts may use immutable `AgentPrompts` via
`AppState::with_agent_prompts` before sharing a host or loading a Space. Only
section A can be replaced; the compiled KIP reference prefix stays intact.
Runtime policies are per-Space `MemoryPolicy` values. Neither configuration
uses a process-global mutable override, and experiments pin actual instance
prompt content in their manifests.

Agent prompts in `anda_brain/assets/` are part of runtime behavior. Edit them
only when the task calls for prompt behavior changes, and describe the intended
agent behavior clearly in the diff. Avoid prompt edits as a workaround for a
code bug.

Each `Brain{Formation,Recall,Maintenance}.md` — in `anda_brain/assets/` and in
`anda-brain-worker/assets/` — is two halves. Everything above `# A.` is the KIP
2.0 reference policy vendored from `anda-db/rs/anda_kip/brain/`; everything from
`# A.` down is that deployment's own contract. **Do not hand-edit the reference
half**: run

```bash
node scripts/sync-kip-assets.mjs
pnpm --filter @ldclabs/anda-brain-worker run codegen:prompts
```

which re-copies the reference half from `anda_kip` in all six files, copies the
Worker's verbatim assets (syntax, Profile and the four role/Memory Interface cards),
and leaves every `# A.` section untouched. The script uses a sibling `anda-db`
checkout by default; set `ANDA_KIP_SOURCE` to a downloaded `anda_kip` crate
directory when syncing to a published version. Edit section A by hand; that is
the half that is ours.
