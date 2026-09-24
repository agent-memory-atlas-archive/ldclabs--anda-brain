# KIP conformance adapter

`adapter.mjs` is an **engine** adapter for the KIP repository's harness runner
(`conformance/runner.mjs`: `describe / seed / harness / inspect`). It drives a real
`anda_brain` process through the Memory Interface HTTP binding and keeps every
request, response and receipt as raw evidence.

Each fixture gets a fresh process and database (auth disabled, loopback only), so a
scenario can restart the host. The process is pointed at a controlled model served by
the adapter: every chat completion answers `done`, so Formation completes and forms
nothing, and a completion whose prompt carries a hold marker waits until the scenario
releases it. No model provider is contacted and nothing is billed.

## What it exercises

| Vector | Exercised | Observations come from |
| --- | --- | --- |
| KIP2-MIF-002 persisted input is not processed memory | yes | a held Formation, `after` barriers before and after |
| KIP2-MIF-005 intake retries survive restart | yes | replays across two process restarts, conflicting key reuse, receipts, Space info |
| KIP2-REL-013 session barrier retention | yes | a held Formation, a checkpointed host session, a foreign Space's receipt |
| every other MIF / REL vector | **not run** | they depend on what a model extracts (facts, corrections, preferences, repairs, reminders) |

The other scenarios are refused with an explicit "not run" error rather than
approximated: a controlled model that forms nothing cannot show that a new fact is
recallable or that a correction changes an answer, and a real-model run needs a
provider, credentials and cost accounting that this repository does not supply.
Mechanism coverage of those paths lives in the Rust tests
(`anda_brain/src/memory_interface/tests.rs`), which script the model inside the process.

Because the KIP runner stops a suite at the first harness error, run the implemented
vectors with the included script:

```sh
cargo build -p anda_brain --features mcp,wiki
KIP_ROOT=/abs/path/KIP ANDA_BRAIN_BIN=$PWD/target/debug/anda_brain \
  node anda_brain/conformance/run-deterministic.mjs
```

The report keeps `profiles_claimed` empty and states partial coverage, as the runner
requires. It is mechanism evidence for these three scenarios only.
