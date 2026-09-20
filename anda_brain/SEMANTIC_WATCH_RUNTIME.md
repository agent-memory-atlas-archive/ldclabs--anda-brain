# Semantic Watch runtime

**[English](SEMANTIC_WATCH_RUNTIME.md) | [中文](SEMANTIC_WATCH_RUNTIME_cn.md)**

The semantic Watch runtime connects Brain to Nexus 0.13.1's protected prepare/read/commit APIs. Text and
mixed selector/text Watches use an explicitly installed evaluator. The model judges
individual event transitions; Nexus alone validates coverage, permissions, basis,
page digest, Watch version and arm generation. Without a binding they remain blocked.
A successful model call, or a model's completeness claim, never proves coverage.

## Startup

Use the per-Space `semantic` field of `BRAIN_RUNTIME_CONFIG`, before loading any
Space. [semantic.runtime.example.json](semantic.runtime.example.json) is a complete
configuration template with automatic model calls disabled. Replace the model and
full Chat Completions endpoint, supply the named environment secret, and use a
versioned model deployment. The response's `model` must exactly match the configured
identifier. The resolved HTTP client also verifies the request pin before sending;
changing a cloned binding cannot relabel an old client or endpoint. BYOK, Recall,
Formation and Maintenance models are not fallback evaluators.
This adapter is available in the lean library; it does not require `learning`.

The compiled `OpenAiWatchConfig` adapter sends one non-streaming JSON-only request
with no tools, temperature 0, a fixed prompt, a complete immutable page and an output
limit. HTTPS is required except on loopback; redirects are refused. Provider errors,
refusal, a different model, non-`stop` finish, malformed/extra fields, missing or
duplicate judgments, mismatched pins and over-budget output all defer coverage.
The secret is resolved at startup and is neither part of the pin nor persisted.
Custom trusted Rust implementations use `SemanticBindings` / `SemanticEvaluator`;
the same host validation applies. They must consume the supplied counted request
without adding hidden history or substituting implementations under an existing pin.

`principal` is a trusted host service identity, separate from API callers and outcome
observers. With action/inbox bindings it must be the same attention controller that
arms the Watch. `bootstrap:true` can provision read/read_history/read_governance_history,
create/update/derive/maintain grants once. Revoked grants are never restored on restart.
`bootstrap:false` requires those grants to be provisioned explicitly. Model output
cannot supply authentication, change a pin, re-arm a Watch, or dispatch an action.

## Budgets and scheduling

| Limit | Default | Allowed maximum |
| --- | ---: | ---: |
| Watches/pages per pass | 2 | 4 |
| Native change envelopes per page | 32 | 200 |
| Candidate transitions per page | 32 | 64 |
| Input tokens per model request | 16,384 | 65,536 |
| Output tokens per model request | 4,096 | 16,384 |
| Cumulative input tokens per pass | 32,768 | 262,144 |
| Model callback time | 15 s | 60 s |
| Model-work pass time | 30 s | 60 s |
| Retry delay | 60 s | 1 hour |
| Model attempts per page | 2 | 4 |

These are operational caps, not empirical quality thresholds. Input is counted over
the exact serialized JSON request using `o200k_base@tiktoken-rs-0.12.0`; output is
counted over the returned content. This contract does not claim provider billing or
hidden provider-template token accounting. HTTP response bytes are capped at 512 KiB,
model JSON content at 256 KiB. Pages are never truncated to fit: excess candidates or
input remain blocked. An empty authorized candidate page needs no model call; only
Nexus's native coverage proof permits advancement. Unknown anywhere blocks the whole
page, including a positive match elsewhere. Silence requires proven no-match coverage
through the native fixed deadline; completing a pre-deadline model call later does
not expand that frozen page's time interval.

Each Space has one serialized semantic pass. A host directory admits at most four
semantic passes concurrently, separately from HTTP/MCP LLM route concurrency.
The structured Watch scan and wake processing do not wait for semantic model I/O;
semantic work has its own persistent cursor. Automatic admission requires the host,
attention registration and `contract.automatic` switches. Disabled or unconfigured
hosts make no automatic semantic model calls. Only admitted model callbacks are
bounded by their timeout; native/storage writes drain to completion, so a slow store
can extend the pass's wall time. Shutdown/eviction owns and drains admitted work.

## Recovery and operator APIs

| Trusted Rust API | Purpose |
| --- | --- |
| `Space::attention().semantic()` | Optional installed runtime |
| `SemanticRuntime::run_once()` | One explicit bounded pass with enabled attention registration |
| `status()` / `progress(watch_ref)` | Configuration/last pass or this Watch's retained page/evaluation refs |
| `retry(watch_ref)` | Operator-reviewed retry after a page exhausts attempts; same page and evaluator |
| `AttentionRuntime::configuration()` | Read native configuration plus its CAS version |
| `reconfigure_evaluator(expected_version)` | Explicitly install the new binding's evaluator pin, or remove it if no binding is installed |
| `AttentionRuntime::set_enabled(false)` | Stop further admission; already admitted work drains |

For an existing Space, install the new startup binding, inspect `configuration()`,
then explicitly call `reconfigure_evaluator` with that native CAS version. Review
and re-arm affected Watches using their current native element versions. Updating
configuration does not rewrite old arms or prove the observation gap. All native
attention pins share a configuration basis: evaluator changes may require review of
structured arms and existing action work too. Policy, action binding and scope
migration are deliberately outside this evaluator-only operation. A native basis or
history failure stays blocked; a retry does not replace missing evidence.

Preparation keys are stable for the exact scope/pin/Watch version/generation.
Before committing, Brain stores the full validated response as a governed native
Artifact bound to every page source. Its off-graph CAS journal stores only refs,
versions, counters and bounded status codes. Commit retries load that exact response
and reuse its evaluation key; a lost native ACK or later host checkpoint cannot
produce a second firing or require another model call. Source purge and current
permissions also govern replay material. No raw page or rationale is copied into the
Brain directory. A caller dropping its future does not cancel an admitted commit.

Runtime HTTP/MCP status adds `semantic_attention`. It reports installed/configured,
automatic eligibility, running state, pin and a bounded reason. Global last-pass
counts are available only to configured auditors; ordinary callers receive
`last_pass:null`. These counts are not complete cognitive or change-stream inventory.
The model-facing host `memory_runtime/status` can inspect the trusted host status;
no new model/HTTP/MCP setter or evaluator endpoint is exposed.

## Validation boundary

Native fault, lifecycle, budget and localhost provider tests verify the mechanism.
They do not measure real semantic precision/recall. Validate the pinned model and
prompt on representative text/mixed Watches in MIB before enabling production
silence reminders. A fixed model name/pin records the deployment contract; it does
not independently prove remote weights or provider behavior. No Assertion confidence,
BELIEF result, Skill standing, utility or execution authority is changed by a judgment.
