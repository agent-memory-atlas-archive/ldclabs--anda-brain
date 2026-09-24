# migrate-draft-space

A one-off tool that moves a Space written under the `kip://profiles/cognitive-memory`
**2.1.0 draft** (Brain 0.12.1 / Nexus 0.13.4 releases) onto a fresh KIP 2.0 Nexus with
`cognitive-memory@2.0.0`. The 0.14 engine does not open such a Space in place: the draft
artifact and the Schema Environments that name it would stay installed, a later official
`cognitive-memory@2.1.0` would fail with `DigestMismatch`, and exact `@2.1.0/…` references
would be reinterpreted under it.

It is a standalone crate, outside the repository workspace, because it links both engines:
the 0.13 engine that wrote the Space (read side) and the 0.14 engine it is rebuilt on
(write side). Until 0.14 is published the write side builds from the sibling `anda-db`
checkout, as the workspace does.

## What it does

1. **Export (0.13).** Every element of the Space in any storage state — Concepts,
   Propositions, Evidence, Assertions, Activities — as one selective Capsule in id order,
   plus what a Capsule import does not carry: each Concept's Space-local key, each
   element's storage state and the Space's self Concept.
2. **Transform.** References to `cognitive-memory@2.1.0/…` (types, predicates, structural
   and facet members) move to `@2.0.0`. The draft `Preference` type does not exist in the
   Profile: each option Concept becomes the option kind you choose for it, defined in the
   Space's draft vocabulary (`prefers` is functional by object type, so options of one kind
   compete and options of different kinds coexist). The draft's written `derived_from`
   member, which 2.0.0 computes, is kept as the `extraction` Activity that produced the
   Concept. `kip://legacy/nexus@1.1.0` and its elements are carried unchanged.
3. **Rebuild (0.14).** The Space database's Nexus collections (elements, versions,
   transactions, Schema Packages and Environments, control records, exposure log, governance
   and the `kip_legacy_v1` staging) are dropped and a fresh Nexus is bootstrapped in the same
   database; the host's own collections (conversations, usage ledger, wiki, …) and database
   extensions are kept. The legacy package and `cognitive-memory@2.0.0` are installed and
   activated, the option kinds are `DEFINE`d, and the Capsule is imported in
   dependency-ordered chunks that fit one transaction. Ids are preserved, keys and archived
   states are restored, and the self Concept is designated again.
4. **Verify.** A census of every kind, storage state and type is compared with the export.

Assertion attribution, `asserted_at`, validity, mode, confidence, stance, lifecycle and
Evidence are carried as recorded; nothing is invented. Element version history, engine
origin and the old transaction log are not carried (they describe the old Nexus); the old
database remains the record of them. Imported Evidence gets an import client key in place
of its original one.

## Running it

Stop the host, keep a backup, and run on a copy:

```sh
cp -R ~/.anda/db ~/.anda/db-2.1.0-draft          # the read-only rollback copy
cp -R ~/.anda/db /tmp/anda-db-migrating
cd tools/migrate-draft-space
cargo run --release -- --db /tmp/anda-db-migrating --work /tmp/migrate-work --list-options
# fill each option's "type" (PascalCase kind) and "description" in
# /tmp/migrate-work/types.template.json, save it as types.json (drop the other fields)
cargo run --release -- --db /tmp/anda-db-migrating --work /tmp/migrate-work --types /tmp/migrate-work/types.json
```

`--space` names the Space database (default `anda_bot`). The export is kept in
`--work/export.json` and reused by later runs; always rebuild from a fresh copy. The run
writes `report.json` (dropped and kept collections, packages, defined types, id changes,
lineage Activities, restored keys and states, warnings) and `census.json`. Swap the migrated
directory in only after checking them, and start the host on the new stack.

The storage settings match Anda Bot's and Anda Brain's; `DBConfig` storage values are
not read from the database.
