import { clearMaintenanceContext } from './maintenance.js'
/** Trusted embedding-host contracts. Never expose caller identity from an HTTP body. */
import {
  ANONYMOUS_PRINCIPAL, contentDigest, formatElementId, isJsonMap, parseElementId,
  parseKip, requirePermitted, spaceResource,
  type AuthContext, type CognitiveNexus, type Element, type EvidenceRow,
  type Json, type JsonMap, type Session, type WhereClause, type ObjectMatcher, type IngestContext,
} from '@ldclabs/kip-do'
import type { KipOperation } from './kip.js'

export interface SourceIdentity { key: string; parents?: string[] }
export interface RecordSource {
  evidence_id: string
  payload_digest: string | null
  origin: string | null
  message_index: number | null
  product_operation: string | null
  observed_at: string | null
}
export interface MemoryRecord {
  id: string; revision: number; proposition_id: string
  actor_id: string | null; actor_key: string | null
  subject: Json; predicate: string; object: Json
  text: string; subject_label: string; object_label: string
  stance: string; status: string; storage_state: string
  asserted_at: string | null; valid_from: string | null; valid_until: string | null
  updated_at: string; sources: RecordSource[]; sources_complete: boolean
}
/**
 * Which history a change writes (Spec §14.2, Memory Interface §4): `correct`
 * supersedes the caller's own wrong claim and keeps the interval it covered;
 * `world_change` adds one Assertion from now and lets temporal succession end
 * the old value; `misrecorded` needs recording repair, which this engine does
 * not provide, so it is refused rather than written as either of the others.
 */
export interface ChangeInput {
  operation_id: string; record_id: string; expected_revision: number
  kind: 'correct' | 'world_change' | 'misrecorded' | 'suppress' | 'delete'; new_value?: string
}
const revises = (kind: ChangeInput['kind']): boolean => kind === 'correct' || kind === 'world_change'
interface Target { id: string; revision: number; kind: string }
export interface ChangePreview {
  record: MemoryRecord; new_value: string | null; targets: Target[]
  excluded_sources: string[]; resets_processing_context: boolean; scope: string
}
export interface ChangeReceipt {
  schema_version: 1; operation_id: string; operation_key: string; caller: string
  state: 'prepared' | 'committing' | 'reconciling' | 'confirmed' | 'discarded'
  preview_digest: string; expires_at: number; preview: ChangePreview
  replacement_record: string | null; source_evidence: string | null; error: string | null
}
interface StoredChange {
  input_digest: string; kind: ChangeInput['kind']; auth: AuthContext
  receipt: ChangeReceipt; requests: KipOperation[]; completed: string[]
}
interface ControlState { version: 1; epoch: number; suppressed: string[]; pending: string | null }
export interface RecordWatch {
  operation_id: string; watch_id: string; target_id: string; state: string; digest: string
}
const PREFIX = 'anda-brain:product:v1:'
const CONTROL = PREFIX + 'control'
const CHANGE = PREFIX + 'change:'
const SOURCE = PREFIX + 'source:'
const EVIDENCE = PREFIX + 'evidence:'
const WATCH = PREFIX + 'watch:'
const SCRUB = PREFIX + 'pending_scrub'
interface PendingScrub { ids: string[]; except?: string }
const digest = (value: unknown): string => contentDigest(JSON.parse(JSON.stringify(value)) as Json)
const fail = (reason: string): never => { throw new Error(reason) }

export function sourceKeys(source: SourceIdentity): string[] {
  if (!source || !Array.isArray(source.parents ?? []) || (source.parents?.length ?? 0) > 16) fail('invalid_source')
  const keys = [source.key, ...(source.parents ?? [])]
  if (keys.some(key => typeof key !== 'string' || !key || new TextEncoder().encode(key).length > 512 || /\p{Cc}/u.test(key))) fail('invalid_source')
  return [...new Set(keys)].sort()
}

/** Only current active reads may feed an agent after a managed change. */
export function assertCurrentOperations(operations: readonly KipOperation[]): void {
  const activeMatcher = (matcher: ObjectMatcher | null) => {
    if (matcher && Object.keys(matcher).some(key => key === 'state' || key.startsWith('state.'))) fail('inactive_memory_disabled')
  }
  function activeClauses(clauses: readonly WhereClause[]): void {
    for (const clause of clauses) {
      if ('Not' in clause) activeClauses(clause.Not)
      else if ('Optional' in clause) activeClauses(clause.Optional)
      else if ('Union' in clause) activeClauses(clause.Union)
      else if ('Concept' in clause) activeMatcher(clause.Concept.matcher)
      else if ('Assertion' in clause) activeMatcher(clause.Assertion.matcher)
      else if ('Evidence' in clause) activeMatcher(clause.Evidence.matcher)
      else if ('Activity' in clause) activeMatcher(clause.Activity.matcher)
    }
  }
  for (const op of operations) {
    const command = parseKip(op.command)
    if ('Kql' in command) {
      if (command.Kql.as_of || command.Kql.cursor) fail('historical_memory_disabled')
      activeClauses(command.Kql.where_clauses)
    } else if ('Meta' in command) {
      // Technical owner audit APIs remain separate. Continuation cursors are
      // conservatively disabled until the engine exports a current-floor gate.
      if (!('Search' in command.Meta)) throw new Error('historical_memory_disabled')
      if (command.Meta.Search.as_of_seq || command.Meta.Search.cursor) fail('historical_memory_disabled')
    } else {
      for (const clause of command.Kml.clauses) {
        if ('UpsertConcept' in clause) activeMatcher(clause.UpsertConcept.match)
        const body = Object.values(clause)[0] as {where_clauses?:WhereClause[] | null}
        if (body.where_clauses) activeClauses(body.where_clauses)
      }
    }
  }
}

/** All mutations are synchronous inside one DO. Journal before each native write. */
export class MemoryProduct {
  constructor(private nexus: CognitiveNexus, private storage: DurableObjectStorage, private recipient?: string) {}
  private get kv() { return this.storage.kv }
  private governedResultLimit: number | null | undefined
  state(): ControlState {
    const state = this.kv.get<ControlState>(CONTROL) ?? { version: 1, epoch: 0, suppressed: [], pending: null }
    if (state.version !== 1) fail('unsupported_product_state')
    return state
  }
  check(epoch: number): void {
    const state = this.state()
    if (state.pending || this.kv.get(SCRUB)) fail('memory_change_pending')
    if (epoch !== state.epoch) fail('memory_changed_rebuild_context')
  }
  begin(source?: SourceIdentity, origin?: string): number {
    const state = this.state()
    this.check(state.epoch)
    if (source) {
      const keys = sourceKeys(source)
      if (keys.some(key => state.suppressed.includes(key))) fail('source_suppressed')
      if (origin) {
        if (!/^formation:sha256:[a-f0-9]{64}$/.test(origin)) fail('invalid_source')
        const key = SOURCE + origin
        const old = this.kv.get<SourceIdentity>(key)
        if (old && digest(old) !== digest(source)) fail('source_identity_conflict')
        this.kv.put(key, source)
      }
    }
    return state.epoch
  }
  /** Bind only the host's captured payloads, not model-authored client keys. */
  capture(ingest?: IngestContext): void {
    for (const evidence of ingest?.evidence ?? []) {
      const key = evidence.client_key
      if (!key || !/^formation:sha256:[a-f0-9]{64}:[1-9]\d*$/.test(key)) continue
      const binding = {payload_digest:digest(evidence.payload), observed_at:evidence.observed_at ?? null}
      const old = this.kv.get(EVIDENCE + key)
      if (old && digest(old) !== digest(binding)) fail('source_identity_conflict')
      this.kv.put(EVIDENCE + key, binding)
    }
  }
  invalidate(): void {
    const state = this.state()
    this.check(state.epoch)
    state.epoch += 1
    if (!Number.isSafeInteger(state.epoch)) fail('epoch_exhausted')
    this.kv.put(CONTROL, state)
    clearMaintenanceContext(this.kv)
  }
  private session(auth: AuthContext): Session {
    if (!auth?.principal_id || auth.principal_id === ANONYMOUS_PRINCIPAL) fail('unauthorized')
    const session = this.nexus.session(auth)
    requirePermitted(session.effectiveAuthority().authorize('read', spaceResource(), auth))
    return session
  }
  private load(session: Session, id: string): Element {
    const row = this.nexus.store.load(parseElementId(id))
    if (!row || row.row.space !== this.nexus.space) return fail('not_found')
    const authority = session.effectiveAuthority()
    const spaceLimit = authority.authorize('read', spaceResource(), session.auth).constraints.max_results
    const visibility = authority.mayRead(row, session.auth)
    if (!visibility?.content || visibility.constraints.fields.length ||
        spaceLimit === 0 || visibility.constraints.max_results === 0) return fail('not_found')
    if (this.governedResultLimit !== undefined && visibility.constraints.max_results !== null) {
      this.governedResultLimit = this.governedResultLimit === null
        ? visibility.constraints.max_results
        : Math.min(this.governedResultLimit, visibility.constraints.max_results)
    }
    return row
  }
  source(auth: AuthContext, id: string): RecordSource {
    const row = this.load(this.session(auth), id)
    if (row.kind !== 'Evidence' || row.row.state === 'purged') return fail('not_found')
    return this.sourceOf(row.row)
  }
  private sourceOf(row: EvidenceRow): RecordSource {
    const formation = /^(formation:sha256:[a-f0-9]{64}):([1-9]\d*)$/.exec(row.client_key)
    const operation = /^memory-product:([a-f0-9]{64}):input$/.exec(row.client_key)?.[1] ?? null
    return {
      evidence_id: `E-${row.id}`,
      payload_digest: row.state !== 'purged' && row.payload_mode === 'inline' ? digest(row.payload_inline) : null,
      origin: formation?.[1] ?? null,
      message_index: formation ? Number(formation[2]) - 1 : null,
      product_operation: operation, observed_at: row.observed_at || null,
    }
  }
  record(auth: AuthContext, id: string): MemoryRecord {
    const session = this.session(auth)
    const element = this.load(session, id)
    if (element.kind !== 'Assertion' || element.row.state === 'purged') return fail('not_found')
    const row = element.row
    const proposition = this.load(session, row.proposition_id)
    if (proposition.kind !== 'Proposition' || proposition.row.state === 'purged') return fail('not_found')
    const p = proposition.row
    const actorId = typeof row.asserted_by.id === 'string' ? row.asserted_by.id : null
    const actor = actorId ? this.load(session, actorId) : null
    const sources = row.evidence_refs.map(ref => {
      const evidence = this.load(session, ref.id)
      if (evidence.kind !== 'Evidence') return fail('invalid_evidence')
      return this.sourceOf(evidence.row)
    })
    const label = (endpoint: Json): string => {
      if (isJsonMap(endpoint) && typeof endpoint.id === 'string') {
        const target = this.load(session, endpoint.id)
        if (target.kind === 'Concept') return target.row.name.slice(0, 512)
      }
      return JSON.stringify(endpoint).slice(0, 512)
    }
    const subject = label(p.subject), object = label(p.object)
    return {
      id, revision: row.version, proposition_id: row.proposition_id,
      actor_id: actorId, actor_key: actor?.kind === 'Concept' ? actor.row.key : null,
      subject: p.subject, predicate: p.predicate_ref, object: p.object,
      text: `${subject} · ${p.predicate_ref.split('/').at(-1)} · ${object}`,
      subject_label: subject, object_label: object, stance: row.stance, status: row.status,
      storage_state: row.state, asserted_at: row.asserted_at || null,
      valid_from: row.valid_from || null, valid_until: row.valid_until || null,
      updated_at: row.updated_at, sources,
      sources_complete: sources.length > 0 && sources.every(source => {
        try { return source.payload_digest !== null && this.keysForSource(source).length > 0 } catch { return false }
      }),
    }
  }
  records(auth: AuthContext, before = Number.MAX_SAFE_INTEGER, limit = 20) {
    const session = this.session(auth)
    if (!Number.isSafeInteger(before) || before < 1 || !Number.isInteger(limit) || limit < 1 || limit > 50) fail('invalid_request')
    this.governedResultLimit = session.effectiveAuthority()
      .authorize('read', spaceResource(), auth).constraints.max_results
    if (this.governedResultLimit === 0) return { records: [], next_cursor: null, complete: false }
    const ids = this.storage.sql.exec<{ id: number }>(
      'SELECT id FROM assertions WHERE space = ? AND id < ? ORDER BY id DESC LIMIT ?', this.nexus.space, before, limit,
    ).toArray()
    const records: MemoryRecord[] = []
    let complete = ids.length < limit
    let lastConsumed: number | null = null
    for (const { id } of ids) {
      try {
        const record = this.record(auth, `A-${id}`)
        if (this.governedResultLimit !== null && records.length >= this.governedResultLimit) {
          complete = false
          break
        }
        records.push(record)
        lastConsumed = id
      } catch {
        complete = false
        lastConsumed = id
      }
      if (this.governedResultLimit !== null && records.length >= this.governedResultLimit) {
        complete = false
        break
      }
    }
    return { records, next_cursor: complete ? null : lastConsumed, complete }
  }
  private keysForSource(source: RecordSource): string[] {
    if (source.origin) {
      const identity = this.kv.get<SourceIdentity>(SOURCE + source.origin)
      const binding = this.kv.get<{payload_digest:string;observed_at:string | null}>(EVIDENCE + `${source.origin}:${(source.message_index ?? -1) + 1}`)
      if (identity && binding?.payload_digest === source.payload_digest && binding?.observed_at === source.observed_at) return [...sourceKeys(identity), source.origin]
    }
    if (source.product_operation) {
      const change = this.kv.get<StoredChange>(CHANGE + source.product_operation)
      if (change?.receipt.state === 'confirmed' && change.receipt.source_evidence === source.evidence_id &&
          digest(change.requests[0]?.parameters?.statement ?? null) === source.payload_digest) return [`product-change:${source.product_operation}`]
    }
    return fail('unsupported_scope')
  }
  private preview(auth: AuthContext, input: ChangeInput): ChangePreview {
    const session = this.session(auth)
    const record = this.record(auth, input.record_id)
    if (record.revision !== input.expected_revision) fail('revision_conflict')
    if (!record.sources_complete) fail('unsupported_scope')
    const sources = new Set(record.sources.flatMap(source => this.keysForSource(source)))
    let targets: Target[]
    if (revises(input.kind)) {
      if (record.status !== 'active' || record.storage_state !== 'active' || record.stance !== 'support' || record.actor_key !== auth.principal_id) fail('unsupported_scope')
      if (typeof input.new_value !== 'string' || !input.new_value.trim() || new TextEncoder().encode(input.new_value).length > 8192 || input.new_value === record.object_label) fail('invalid_request')
      targets = [{ id: record.id, revision: record.revision, kind: 'assertion' }]
    } else {
      if (input.new_value !== undefined) fail('invalid_request')
      const pending = [record.proposition_id], seen = new Set<string>(), found: Target[] = []
      while (pending.length) {
        const id = pending.pop()!
        if (seen.has(id)) continue
        seen.add(id)
        if (seen.size > 128) fail('unsupported_scope')
        const element = this.load(session, id)
        if (element.row.state === 'purged') continue
        if (element.kind === 'Concept' || element.row.retention.legal_hold === true) fail('unsupported_scope')
        if (element.kind === 'Assertion') {
          pending.push(element.row.proposition_id, ...element.row.evidence_refs.map(ref => ref.id))
        }
        if (element.kind === 'Evidence') for (const key of this.keysForSource(this.sourceOf(element.row))) sources.add(key)
        found.push({ id, revision: element.row.version, kind: element.kind.toLowerCase() })
        pending.push(...this.nexus.store.referrers(this.nexus.space, parseElementId(id)).map(ref => formatElementId(ref.from)))
      }
      targets = found.sort((a, b) => a.id.localeCompare(b.id))
    }
    return { record, new_value: input.new_value ?? null, targets, excluded_sources: [...sources].sort(),
      resets_processing_context: true,
      scope: 'Selected claims, cited input messages and recorded dependents. Source identities stop contributing memory; in-flight agent contexts are invalidated. Independent records, external chats/files/logs/backups and already delivered context are not erased.' }
  }
  private key(auth: AuthContext, id: string): string {
    this.session(auth)
    if (!/^[A-Za-z0-9_-]{1,128}$/.test(id)) return fail('invalid_request')
    return digest({ caller: auth.principal_id, operation_id: id }).slice(7)
  }
  prepare(auth: AuthContext, input: ChangeInput): ChangeReceipt {
    if (!input || Object.keys(input).some(key => !['operation_id', 'record_id', 'expected_revision', 'kind', 'new_value'].includes(key)) ||
        !['correct', 'world_change', 'misrecorded', 'suppress', 'delete'].includes(input.kind) || !Number.isSafeInteger(input.expected_revision) || input.expected_revision < 1) fail('invalid_request')
    if (input.kind === 'misrecorded') fail('unsupported_capability')
    const key = this.key(auth, input.operation_id), inputDigest = digest(input)
    const old = this.kv.get<StoredChange>(CHANGE + key)
    if (old) {
      this.expire(old, key)
      if (old.receipt.state === 'discarded') fail('preview_expired')
      if (old.input_digest !== inputDigest) fail('idempotency_conflict')
      return this.change(auth, input.operation_id)
    }
    this.check(this.state().epoch)
    const preview = this.preview(auth, input)
    const requests = this.requests(auth, key, input, preview)
    // Native dry-run validates schema, current authority and legal holds before
    // storing a reviewable proposal. It never consumes the idempotency key.
    for (const request of requests) {
      const command = parseKip(request.command)
      if (!('Kml' in command)) throw new Error('invalid_request')
      this.session(auth).mutate(command.Kml, request.parameters, { dryRun: true })
    }
    const receipt: ChangeReceipt = { schema_version: 1, operation_id: input.operation_id, operation_key: key,
      caller: auth.principal_id, state: 'prepared', preview_digest: digest(preview), expires_at: Date.now() + 600_000,
      preview, replacement_record: null, source_evidence: null, error: null }
    this.kv.put(CHANGE + key, { input_digest: inputDigest, kind: input.kind, auth, receipt, requests, completed: [] } satisfies StoredChange)
    return receipt
  }
  change(auth: AuthContext, id: string): ChangeReceipt {
    const stored = this.kv.get<StoredChange>(CHANGE + this.key(auth, id)) ?? fail('not_found')
    this.expire(stored, stored.receipt.operation_key)
    // A stored preview must not bypass a later read revocation.
    const session = this.session(auth)
    const record = stored.receipt.preview.record
    const ids = new Set([...stored.receipt.preview.targets.map(target => target.id), record.proposition_id,
      ...record.sources.map(source => source.evidence_id)])
    for (const endpoint of [record.subject, record.object, {id:record.actor_id}]) {
      if (isJsonMap(endpoint) && typeof endpoint.id === 'string') ids.add(endpoint.id)
    }
    for (const id of ids) this.load(session, id)
    return stored.receipt
  }
  discard(auth: AuthContext, id: string): void {
    const key = this.key(auth, id), stored = this.kv.get<StoredChange>(CHANGE + key) ?? fail('not_found')
    if (!['prepared', 'discarded'].includes(stored.receipt.state) || this.state().pending === key) fail('memory_change_pending')
    stored.receipt.state = 'discarded'
    clearContent(stored)
    this.kv.put(CHANGE + key, stored)
  }
  commit(auth: AuthContext, id: string, previewDigest: string): ChangeReceipt {
    const key = this.key(auth, id), stored = this.kv.get<StoredChange>(CHANGE + key) ?? fail('not_found')
    // Even a pending retry must not return an old preview under narrowed reads.
    this.change(auth, id)
    if (stored.receipt.preview_digest !== previewDigest) fail('revision_conflict')
    const control = this.state()
    if (stored.receipt.state === 'confirmed' && !control.pending) return this.change(auth, id)
    if (control.pending && control.pending !== key) fail('memory_change_pending')
    if (stored.receipt.state === 'discarded') fail('preview_expired')
    if (!control.pending) {
      if (stored.receipt.state !== 'prepared' || Date.now() > stored.receipt.expires_at) fail('preview_expired')
      const input: ChangeInput = { operation_id: id, kind: stored.kind, record_id: stored.receipt.preview.record.id,
        expected_revision: stored.receipt.preview.record.revision,
        ...(revises(stored.kind) ? { new_value: stored.receipt.preview.new_value! } : {}) }
      if (digest(this.preview(auth, input)) !== stored.receipt.preview_digest) fail('revision_conflict')
      control.epoch += 1
      if (!Number.isSafeInteger(control.epoch)) fail('epoch_exhausted')
      control.suppressed = [...new Set([...control.suppressed, ...stored.receipt.preview.excluded_sources])]
      if (control.suppressed.length > 100_000) fail('source_capacity_exhausted')
      control.pending = key
      // Persist the fence before the first native write; reload can resume even
      // if interrupted before the receipt moves from prepared to committing.
      this.kv.put(CONTROL, control)
    }
    stored.auth = auth
    stored.receipt.state = 'committing'
    this.kv.put(CHANGE + key, stored)
    return this.apply(key)
  }
  recover(): void {
    const scrub = this.kv.get<PendingScrub>(SCRUB)
    if (scrub) {
      try { this.scrubChanges(new Set(scrub.ids), scrub.except) }
      catch { fail('memory_change_pending') }
    }
    const key = this.state().pending
    if (key) this.apply(key)
  }
  private apply(key: string): ChangeReceipt {
    const stored = this.kv.get<StoredChange>(CHANGE + key) ?? fail('memory_change_missing')
    try {
      const session = this.session(stored.auth)
      for (const [index, request] of stored.requests.entries()) {
        const target = revises(stored.kind) ? 'correction' : stored.receipt.preview.targets[index]!.id
        if (stored.completed.includes(target)) continue
        const row = target === 'correction' ? null : this.load(session, target)
        const already = row?.row.state === 'purged' || (stored.kind === 'suppress' && row?.row.state === 'archived')
        if (!already) {
          const command = parseKip(request.command)
          if (!('Kml' in command)) throw new Error('invalid_change')
          const outcome = session.mutate(command.Kml, request.parameters, { idempotencyKey: `memory-product:${key}:${target}` })
          if (outcome.status !== 'committed') fail('change_not_committed')
          if (revises(stored.kind)) {
            stored.receipt.replacement_record = outcome.handles.new ?? null
            stored.receipt.source_evidence = outcome.handles.input ?? null
          }
        }
        stored.completed.push(target)
        this.kv.put(CHANGE + key, stored)
      }
      if (revises(stored.kind)) {
        // A correction supersedes the old claim; a world change leaves it
        // active and true for its time.
        const expected = stored.kind === 'correct' ? 'superseded' : 'active'
        if (this.record(stored.auth, stored.receipt.preview.record.id).status !== expected || !stored.receipt.replacement_record) fail('verification_incomplete')
      } else {
        for (const target of stored.receipt.preview.targets) {
          const state = this.load(session, target.id).row.state
          if (state !== 'purged' && state !== (stored.kind === 'delete' ? 'purged' : 'archived')) fail('verification_incomplete')
        }
      }
      if (stored.kind === 'delete') {
        const erased = new Set(stored.receipt.preview.targets.map(target => target.id))
        this.scrubChanges(erased, key)
        clearContent(stored)
      }
      stored.receipt.state = 'confirmed'
      stored.receipt.error = null
      this.kv.put(CHANGE + key, stored)
      // Clear durable assessment context that may contain now-retired roots.
      clearMaintenanceContext(this.kv)
      this.kv.put(CONTROL, { ...this.state(), pending: null })
    } catch {
      // Provider/native diagnostics may contain retired bytes; keep only a stable
      // reason in product status. Retry through commit/reload with current grants.
      stored.receipt.state = 'reconciling'
      stored.receipt.error = 'native_change_requires_reconciliation'
      this.kv.put(CHANGE + key, stored)
    }
    return stored.receipt
  }
  /** One bounded page at a time; never materialize the entire preview journal. */
  private *changes(): Generator<[string, StoredChange]> {
    let startAfter: string | undefined
    while (true) {
      const page = [...this.kv.list<StoredChange>({ prefix: CHANGE, limit: 50, ...(startAfter ? {startAfter} : {}) })]
      yield* page
      if (page.length < 50) return
      startAfter = page.at(-1)![0]
    }
  }
  private expire(stored: StoredChange, key: string): void {
    if (stored.receipt.state === 'prepared' && stored.receipt.expires_at < Date.now() && this.state().pending !== key) {
      stored.receipt.state = 'discarded'
      clearContent(stored)
      this.kv.put(CHANGE + key, stored)
    }
  }
  /** Erased graph content must not survive in host preview copies. */
  scrubChanges(erased: ReadonlySet<string>, except?: string): void {
    // A failed cleanup must be retried even when the native elements are now
    // purged and a repeated forget consequently has no new changes to report.
    const pending = this.kv.get<PendingScrub>(SCRUB)
    const ids = new Set([...erased, ...(pending?.ids ?? [])])
    this.kv.put(SCRUB, {ids:[...ids], ...(except ? {except} : {})} satisfies PendingScrub)
    for (const [path, related] of this.changes()) {
      if (path === CHANGE + except) continue
      this.expire(related, path.slice(CHANGE.length))
      const record = related.receipt.preview.record
      const oldErased = ids.has(record.id) || ids.has(record.proposition_id) || record.sources.some(source => ids.has(source.evidence_id))
      const replacementErased = ids.has(related.receipt.replacement_record ?? '') || ids.has(related.receipt.source_evidence ?? '')
      if (!oldErased && !replacementErased) continue
      if (related.receipt.state === 'prepared') related.receipt.state = 'discarded'
      if (replacementErased || related.receipt.state !== 'confirmed') clearContent(related)
      else clearRecord(related)
      this.kv.put(path, related)
    }
    this.kv.delete(SCRUB)
  }

  private requests(auth: AuthContext, key: string, input: ChangeInput, preview: ChangePreview): KipOperation[] {
    if (!revises(input.kind)) return preview.targets.map(target => ({
      command: input.kind === 'delete'
        ? 'PURGE :id EXPECT VERSION :version REFERENCE POLICY "tombstone_reference" CONFIRM "PURGE"'
        : 'TRANSITION :id TO "archived" EXPECT VERSION :version',
      parameters: { id: target.id, version: target.revision },
    }))
    const objectId = isJsonMap(preview.record.object) ? preview.record.object.id : null
    if (typeof objectId !== 'string') return fail('unsupported_scope')
    const object = this.load(this.session(auth), objectId)
    if (object.kind !== 'Concept') return fail('unsupported_scope')
    const parameters: JsonMap = { old: input.record_id,
      source_key: `memory-product:${key}:input`, statement: {kind:input.kind,new_value: input.new_value!,previous_record:input.record_id},
      at: new Date().toISOString(), object_type: object.row.schema_ref, new_value: input.new_value!,
      subject: preview.record.subject, predicate: preview.record.predicate, actor: {id: preview.record.actor_id!} }
    if (input.kind === 'correct') {
      // §14.2: a value-only correction keeps the interval it corrects; an
      // absent start becomes the original claim's `{latest: asserted_at}`.
      const from = endpoint(preview.record.valid_from) ??
        (preview.record.asserted_at ? { latest: preview.record.asserted_at } : fail('unsupported_scope'))
      const until = endpoint(preview.record.valid_until)
      return [{ command: `MUTATE {
      CREATE EVIDENCE ?input { CLIENT KEY :source_key SET FIELDS {evidence_class:"user_statement",payload: :statement,observed_at: :at} }
      CREATE CONCEPT ?value { TYPE :object_type NAME :new_value }
      ASSERT ?new (:subject, :predicate, ?value) {by: :actor,mode:"stated",evidence:?input,at: :at,valid: :valid}
      TRANSITION :old TO "superseded" BY ?new EXPECT VERSION :version
      CREATE ACTIVITY ?change { SET FIELDS {activity_class:"belief_revision",status:"completed",started_at: :at,ended_at: :at} SET STRUCTURAL {("inputs", :old) ("inputs", ?input) ("outputs", ?new)} }
    }`, parameters: { ...parameters, version: input.expected_revision,
        valid: until === undefined ? { from } : { from, until } } }]
    }
    // §25.4: one new Assertion from the change, whose time is known no more
    // precisely than "no later than now"; temporal succession ends the old
    // value. Nothing writes the old claim, so the prepared revision check
    // under the product fence is its concurrency guard.
    return [{ command: `MUTATE {
      CREATE EVIDENCE ?input { CLIENT KEY :source_key SET FIELDS {evidence_class:"user_statement",payload: :statement,observed_at: :at} }
      CREATE CONCEPT ?value { TYPE :object_type NAME :new_value }
      ASSERT ?new (:subject, :predicate, ?value) {by: :actor,mode:"stated",evidence:?input,at: :at}
      CREATE ACTIVITY ?change { SET FIELDS {activity_class:"user_memory_change",status:"completed",started_at: :at,ended_at: :at} SET STRUCTURAL {("inputs", :old) ("inputs", ?input) ("outputs", ?new)} }
    }`, parameters }]
  }
  correctionSource(auth: AuthContext, source: RecordSource): string | null {
    if (!source.product_operation || !/^[a-f0-9]{64}$/.test(source.product_operation)) return null
    const current = this.source(auth, source.evidence_id)
    if (digest(current) !== digest(source)) return null
    const stored = this.kv.get<StoredChange>(CHANGE + source.product_operation)
    if (!stored || stored.receipt.caller !== auth.principal_id || stored.receipt.state !== 'confirmed' || stored.receipt.source_evidence !== source.evidence_id) return null
    return digest(stored.requests[0]?.parameters?.statement ?? null) === source.payload_digest ? stored.receipt.preview.new_value : null
  }
  private watchSession(auth: AuthContext): Session {
    if (!this.recipient || auth.principal_id !== this.recipient) return fail('recipient_binding_required')
    return this.session(auth)
  }
  watch(auth: AuthContext, id: string): RecordWatch {
    const session = this.watchSession(auth)
    const row = this.kv.get<RecordWatch>(WATCH + this.key(auth, id)) ?? fail('not_found')
    if (!row.watch_id) return row
    const watch = this.load(session, row.watch_id)
    if (watch.kind !== 'Concept') return fail('not_found')
    return { ...row, state: watch.row.state === 'archived' ? 'cancelled' : String(watch.row.attributes.status ?? 'unknown') }
  }
  createWatch(auth: AuthContext, id: string, target: string, summary: string): RecordWatch {
    const session = this.watchSession(auth), key = this.key(auth, id)
    this.record(auth, target)
    if (!summary || new TextEncoder().encode(summary).length > 4096) fail('invalid_request')
    const hash = digest({ target, summary }), old = this.kv.get<RecordWatch>(WATCH + key)
    if (old) {
      if (old.digest !== hash) fail('idempotency_conflict')
      if (old.state !== 'preparing') return this.watch(auth, id)
    } else {
      this.kv.put(WATCH + key, {operation_id:id,watch_id:'',target_id:target,state:'preparing',digest:hash} satisfies RecordWatch)
    }
    const command = parseKip('CREATE CONCEPT ?watch { TYPE "Watch" CLIENT KEY :key SET ATTRIBUTES {watch_class:"delta",summary: :summary,status:"disarmed",condition:{element: :target}} }')
    if (!('Kml' in command)) return fail('invalid_watch')
    const outcome = session.mutate(command.Kml, { key: `product-watch:${key}`, summary, target }, { idempotencyKey: `product-watch:${key}` })
    const watchId = outcome.handles.watch ?? fail('watch_missing')
    this.kv.put(WATCH + key, {operation_id:id,watch_id:watchId,target_id:target,state:'preparing',digest:hash} satisfies RecordWatch)
    const current = this.load(session, watchId)
    // Retry after arming/cancellation must not create a new generation.
    if (current.row.version === 1) session.armWatch(watchId, 1)
    this.kv.put(WATCH + key, {operation_id:id,watch_id:watchId,target_id:target,state:'unknown',digest:hash} satisfies RecordWatch)
    return this.watch(auth, id)
  }
  advanceWatch(auth: AuthContext, id: string): RecordWatch {
    const session = this.watchSession(auth), row = this.watch(auth, id)
    const current = this.load(session, row.watch_id)
    if (current.kind !== 'Concept' || current.row.state !== 'active' || row.state !== 'armed') return row
    const watchState = Object.entries(current.row.facets).find(([key]) => key.endsWith('/WatchState') || key === 'WatchState')?.[1]
    if (!isJsonMap(watchState) || typeof watchState.arm_generation !== 'number') return fail('watch_generation_missing')
    session.advanceWatch(row.watch_id, current.row.version, watchState.arm_generation, 200)
    return this.watch(auth, id)
  }
  cancelWatch(auth: AuthContext, id: string): RecordWatch {
    const session = this.watchSession(auth), key = this.key(auth, id)
    let row = this.watch(auth, id)
    if (row.state === 'preparing' && !row.watch_id) {
      // Creation may have committed before its id was saved. Resolve that
      // native identity before deciding that there is nothing to archive.
      const native = this.nexus.store.byClientKey('Concept', this.nexus.space, `product-watch:${key}`)
      if (!native) {
        row = { ...row, state: 'cancelled' }
        this.kv.put(WATCH + key, row)
        return row
      }
      if (native.kind !== 'Concept' || !native.row.schema_ref.endsWith('/Watch') ||
          !isJsonMap(native.row.attributes.condition) ||
          native.row.attributes.condition.element !== row.target_id) fail('watch_identity_conflict')
      row = { ...row, watch_id: `C-${native.row.id}` }
      this.load(session, row.watch_id)
      this.kv.put(WATCH + key, row)
    }
    if (row.state !== 'cancelled') {
      const current = this.load(session, row.watch_id)
      session.execute('TRANSITION :id TO "archived" EXPECT VERSION :version', {id: row.watch_id,version:current.row.version})
    }
    return this.watch(auth, id)
  }
}
function clearRecord(stored: StoredChange): void {
  const record = stored.receipt.preview.record
  record.text = record.subject_label = record.object_label = ''
  record.subject = record.object = null
}
function clearContent(stored: StoredChange): void {
  clearRecord(stored)
  stored.receipt.preview.new_value = null
  stored.receipt.error = null
  stored.requests = []
}

/** A stored valid-time endpoint as the wire form: a timestamp or a `{earliest, latest}` bound. */
function endpoint(stored: string | null): Json | undefined {
  if (!stored) return undefined
  if (!stored.startsWith('{')) return stored
  try { return JSON.parse(stored) as Json } catch { return undefined }
}
