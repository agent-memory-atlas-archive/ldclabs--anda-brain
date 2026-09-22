# Trusted memory product contracts

[中文](PRODUCT_cn.md) · [Worker API](README.md) · [Port audit](PORTING_AUDIT.md)

These APIs are Durable Object RPC methods for trusted embedding hosts. They are
not public HTTP routes or model tools. The host must authenticate the user and
supply a native `AuthContext`; never deserialize this argument from user/model
input. An HTTP API key, a semantic actor, and a native Principal are distinct.
Every product call checks current native read authority; native writes recheck
the relevant permission. This adapter creates no Grants or ActorBindings.

## Records and sources

- `productRecords(auth, before?, limit?)`: newest Assertion-backed records, at
  most 50 per page (default 20); `next_cursor` is the exclusive numeric id bound.
  `complete` is false for another page or an unavailable record.
- `productRecord(auth, assertionId)`: proposition, semantic actor, stance,
  epistemic status, storage state, revision, valid times and source references.
  Proposition existence alone never becomes a true personal fact.
- `productSource(auth, evidenceId)` returns a live source reference and payload
  digest. A digest is not a copy of the source; source bytes require an authorized
  Evidence read. Records with missing/purged/external/unregistered sources cannot
  be changed through this managed API.
- `productCorrectionSource(auth, source)` returns the caller's confirmed correction
  text only when its Evidence, operation binding and live payload digest match.

Formation registers the observation's `formation:sha256:…` identity before calling
the model. The default source is that observation, with `source:<contentDigest>`
of `context.source` as a parent when provided. Suppressing that parent also blocks
later observations from the same thread/channel. A trusted host calling
`formMemory(env, brain, input, sourceIdentity)` can supply `{key, parents?}`;
keys are nonempty, at most 512 UTF-8 bytes, without controls, with at most 16 parents.
These identities determine memory admission, not truth, authority or global
request exactly-once behavior. They are shown in the change preview.

## Reviewed changes and recovery

`productPrepare(auth, {operation_id, record_id, expected_revision, kind, new_value?})`
stores a ten-minute preview. Operation ids use 1–128 ASCII letters, digits, `_` or
`-`. `kind` is `correct`, `suppress`, or `delete`. It returns a receipt with exact
versioned targets, excluded sources, scope and `preview_digest`.

`productCommit(auth, operation_id, preview_digest)` revalidates the record, source
closure and current permissions. Retries with the same operation/input recover the
same result; changed input conflicts. `productChange(auth, operation_id)` reads
status; `productDiscard(auth, operation_id)` invalidates an uncommitted preview
and clears its copied content. An expired/discarded preview cannot be committed.

A correction requires an active supporting Assertion whose actor key matches the
verified caller's principal id. Only Concept-valued records supported by the live
schema are accepted. `new_value` is nonblank, at most 8192 UTF-8 bytes. One native
`MUTATE` retracts the old claim and creates new user Evidence, a new typed value,
an attributed Assertion and a provenance Activity; it never overwrites an
Assertion or treats another actor's testimony as the caller's own statement.

Suppression archives and deletion purges the reviewed closure: the Proposition,
Assertions, cited Evidence and recorded referrers, bounded to 128 elements. Shared
Evidence can widen this scope, and its source identities appear in the preview.
Concept-containing closures, unknown sources and legal holds are refused. Semantic
endpoint Concepts are not automatically erased. Independent records, external
chats/files/logs/backups and already delivered context are outside this scope.

Before the first native mutation, the host persists source suppression, a new
processing epoch and the pending operation. Every later agent read, vocabulary
publication, write and final response checks that epoch. Old Formation,
Maintenance and Recall requests must rebuild context; HTTP returns 409 rather
than delivering stale content. There is no persisted model history/Notes cache in
this Worker. Durable maintenance assessment copies are cleared after changes.

Each native step has a stable idempotency key and saved progress. `committing` or
`reconciling` remains unavailable to automatic processing. Subsequent object access
and eviction/reload retry admitted work under current native authority; an
unresolved permission/hold/version failure keeps the fence in place. Never reset
that fence or replay the whole plan to hide an unresolved write. `confirmed`
requires native readback and cleanup of copied erased content in related previews.
Confirmed receipts retain ids/digests; erased content is removed. A surviving
independent correction retains its own source text.

The closure is processed as versioned individual operations using identity stubs,
not an atomic batch. This avoids an engine cascade silently erasing targets outside
the reviewed list. Historical/inactive model selectors, all model continuation
cursors and META other than SEARCH are disabled after a managed change. Technical
owner `execute_kip_readonly` remains an audit API; do not expose it as the model's
read channel. The same warning applies to administrative raw writes.

## Recipient-owned record watches

Set `BRAIN_PRODUCT_RECIPIENT` to an explicitly registered native principal id.
The host still supplies verified authentication and native permissions. This
setting grants no authority and is never inferred from the legacy HTTP API key.

- `productCreateRecordWatch(auth, operation_id, assertionId, summary)` durably
  registers work before creating and arming a structured delta Watch. Summary is
  nonempty and at most 4096 UTF-8 bytes. Retrying resumes the same Watch; it never
  rearms an old generation. A creation interrupted in `preparing` needs the same
  create call to resume.
- `productRecordWatch(auth, operation_id)` reads native state for that recipient.
- `productAdvanceRecordWatch(auth, operation_id)` checks the current overall
  version and WatchState generation and asks the native engine to advance at most
  200 changes. Native authorized coverage decides whether it fires.
- `productCancelRecordWatch(auth, operation_id)` archives the Watch, preserving
  generation and checkpoint. Retrying creation or cancellation never rearms it.

This is a host-polled subscription, without background inbox delivery, arbitrary
callbacks, semantic conditions, business dispatch or automatic grants.

`productStatus()` is trusted host diagnostics: epoch, availability, pending key and
learning readiness. Readiness remains `services_missing`, `supported:false`;
use the Rust learning runtime with explicit executor/observer/source and calibration
bindings. A successful model call or Watch firing is not an independent outcome,
empirical improvement or permission to execute a procedure.
