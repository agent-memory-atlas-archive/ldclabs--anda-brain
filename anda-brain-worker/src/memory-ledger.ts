/**
 * The Durable Object half of the Memory Interface: staged sources, idempotency
 * keys, receipts, scope Concepts, feedback, recording repair, forget, and the
 * retained basis behind each briefing. Mirrors `anda_brain::memory_interface`;
 * the Worker half (`memory.ts`) runs the model passes around it.
 *
 * Everything here is synchronous inside the object, so a key is bound to one
 * receipt without a lock. Formation on this Worker completes within the
 * request, so a receipt is `recorded` only while its pass runs; one whose
 * pass never reported back is failed with an unknown outcome, never left
 * pending forever and never re-run.
 */
import {
  contentDigest,
  isJsonMap,
  parseElementId,
  tryParseElementId,
  type CognitiveNexus,
  type Json,
  type JsonMap,
  type Session,
} from '@ldclabs/kip-do'
import type { MemoryProduct, SourceIdentity } from './product.js'
import type { Message } from './types.js'
import {
  MAX_AFTER,
  MemoryError,
  coverage as coverageOf,
  type AttentionItem,
  type Briefing,
  type ChangeKind,
  type Channels,
  type ChannelState,
  type Disposition,
  type EpistemicStatus,
  type MemoryItem,
  type MemoryRequest,
  type Progress,
  type Receipt,
  type Role,
  type Scope,
} from './memory-wire.js'

const PREFIX = 'anda-brain:memory:v1:'
const SOURCE = PREFIX + 'source:'
const RECEIPT = PREFIX + 'receipt:'
const RECALL = PREFIX + 'recall:'
const PLAN = PREFIX + 'plan:'
/** A pass that has not reported back after this long was interrupted. */
const ABANDONED_MS = 10 * 60_000
const MAX_SOURCE_MESSAGES = 16
const MAX_SOURCE_BYTES = 256 * 1024
const CHANNEL_LIMIT = 32
const SEARCH_LIMIT = 8
const MAX_ITEMS = 128

export interface SourceOrder { stream_ref: string; event_ref: string; ordinal: number; predecessor_receipts?: string[] }
export interface StageSourceInput {
  messages: Message[]; observed_at?: string; kind?: 'message' | 'tool_trace' | 'artifact'
  order?: SourceOrder; idempotency_key: string
}
export interface StagedSourceRef { source_ref: string; source_digest: string; captured_at: string }
export interface StagedSource extends StagedSourceRef {
  namespace: string; kind: string; messages: Message[]; observed_at: string; order?: SourceOrder; erased: boolean
}
export interface ResolvedScope { requested: Scope; task: string | null; contexts: string[] }
export interface MemoryIntent {
  receipt_ref: string; operation: 'observe' | 'revise'; source_ref: string; scope: ResolvedScope
  change_kind?: ChangeKind; target_ref?: string; original_evidence?: string; original_observed_at?: string
}
export interface IntakeRecord {
  receipt: Receipt; namespace: string; intent_digest: string; scope: ResolvedScope
  source_ref?: string; source_digest?: string; intent?: MemoryIntent
  terminal?: Progress; result?: unknown; warnings: string[]; created_at: number
  /** Evidence a Formation pass captured from this intake's source. */
  evidence?: string[]
}
/** What intake decided: a replay of a bound key, or new work to run. */
export type Admission =
  | { replay: IntakeRecord }
  | { record: IntakeRecord; messages: Message[]; observed_at: string; identity: SourceIdentity; intent?: MemoryIntent }
/** What a Formation pass committed, from its results. */
export interface PassTrace { formed: string[]; evidence: string[]; assertions: string[]; max_seq: number | null }

const digest = (value: unknown): string => contentDigest(JSON.parse(JSON.stringify(value ?? null)) as Json)
const hexId = (value: unknown): string => digest(value).slice(7, 47)
const notFound = (what: string): never => { throw new MemoryError('NotFoundOrNotVisible', `${what} not found`) }

function canonicalTimestamp(value: string): string {
  const parsed = Date.parse(value.trim())
  if (!Number.isFinite(parsed) || !/T/.test(value)) throw new MemoryError('InvalidRequestEnvelope', `timestamp ${JSON.stringify(value)} is not an RFC 3339 instant`)
  if (/\.\d{4,}/.test(value)) throw new MemoryError('InvalidRequestEnvelope', `timestamp ${JSON.stringify(value)} is finer than milliseconds`)
  return new Date(parsed).toISOString()
}

const evidenceClass = (role: string): string =>
  role === 'user' ? 'user_statement' : role === 'assistant' ? 'agent_statement' : role === 'tool' ? 'tool_result' : 'message'

const local = (reference: string): string => reference.split('/').at(-1) ?? reference

export class MemoryLedger {
  constructor(
    private readonly nexus: CognitiveNexus,
    private readonly storage: DurableObjectStorage,
    private readonly product: MemoryProduct,
    private readonly session: Session,
  ) {}

  private get kv() { return this.storage.kv }

  seq(): number {
    return this.nexus.spaceRow().seq
  }

  private query(command: string, params: JsonMap = {}): Json[] {
    return this.session.query(command, params)
  }

  // ── Sources ──────────────────────────────────────────────────────────────

  stage(namespace: string, space: string, input: StageSourceInput): StagedSourceRef {
    if (!input || typeof input.idempotency_key !== 'string' || !input.idempotency_key || input.idempotency_key.length > 1024) {
      throw new MemoryError('InvalidRequestEnvelope', 'idempotency_key is 1..=1024 characters')
    }
    if (!Array.isArray(input.messages) || input.messages.length === 0 || input.messages.length > MAX_SOURCE_MESSAGES) {
      throw new MemoryError('InvalidRequestEnvelope', `a staged source holds 1..=${MAX_SOURCE_MESSAGES} messages`)
    }
    if (new TextEncoder().encode(JSON.stringify(input.messages)).byteLength > MAX_SOURCE_BYTES) {
      throw new MemoryError('ResultLimitExceeded', `a staged source is at most ${MAX_SOURCE_BYTES} bytes`)
    }
    const kind = input.kind ?? 'message'
    if (!['message', 'tool_trace', 'artifact'].includes(kind)) throw new MemoryError('InvalidRequestEnvelope', 'unknown source kind')
    if (input.order) {
      const order = input.order
      if (typeof order.stream_ref !== 'string' || !order.stream_ref || typeof order.event_ref !== 'string' || !order.event_ref ||
          !Number.isSafeInteger(order.ordinal) || order.ordinal < 0) {
        throw new MemoryError('InvalidRequestEnvelope', 'a source order names its stream, event and ordinal')
      }
      if ((order.predecessor_receipts ?? []).length > MAX_AFTER) throw new MemoryError('ResultLimitExceeded', 'too many predecessor receipts')
      for (const predecessor of order.predecessor_receipts ?? []) this.record(namespace, predecessor)
    }
    const captured_at = new Date().toISOString()
    const observed_at = input.observed_at === undefined ? captured_at : canonicalTimestamp(input.observed_at)
    const source_digest = digest({ kind, messages: input.messages, observed_at })
    const source_ref = `src-${hexId([namespace, space, 'source', input.idempotency_key])}`
    // Admission precedes capture: excluded bytes are refused before storage.
    const excluded = this.product.state().suppressed
    if ([`memory-source:${source_ref}`, `memory-source-digest:${source_digest}`].some(key => excluded.includes(key))) {
      throw new MemoryError('NotFoundOrNotVisible', 'this source is excluded from memory by an earlier forget')
    }
    const existing = this.kv.get<StagedSource>(SOURCE + source_ref)
    if (existing) {
      if (existing.namespace !== namespace) notFound('source')
      if (existing.source_digest !== source_digest || JSON.stringify(existing.order ?? null) !== JSON.stringify(input.order ?? null)) {
        throw new MemoryError('IdempotencyConflict', 'this staging key was used for different source bytes')
      }
      return { source_ref, source_digest, captured_at: existing.captured_at }
    }
    const staged: StagedSource = {
      source_ref, source_digest, captured_at, namespace, kind, messages: input.messages, observed_at,
      ...(input.order ? { order: input.order } : {}), erased: false,
    }
    this.kv.put(SOURCE + source_ref, staged)
    return { source_ref, source_digest, captured_at }
  }

  stagedSource(namespace: string, sourceRef: string): StagedSource {
    if (!/^src-[a-f0-9]{40}$/.test(sourceRef)) return notFound('source')
    const staged = this.kv.get<StagedSource>(SOURCE + sourceRef)
    if (!staged || staged.namespace !== namespace) return notFound('source')
    return staged
  }

  private resolveSource(namespace: string, sourceRef: string): { ref: string; digest: string; messages: Message[]; observed_at: string; order?: SourceOrder; evidence?: string } {
    if (sourceRef.startsWith('src-')) {
      const staged = this.stagedSource(namespace, sourceRef)
      if (staged.erased) notFound('source')
      return { ref: staged.source_ref, digest: staged.source_digest, messages: staged.messages, observed_at: staged.observed_at, ...(staged.order ? { order: staged.order } : {}) }
    }
    const id = tryParseElementId(sourceRef)
    const row = id ? this.nexus.store.load(id) : null
    if (!row || row.kind !== 'Evidence' || row.row.space !== this.nexus.space || row.row.state !== 'active' || row.row.payload_mode !== 'inline') return notFound('source')
    const payload = row.row.payload_inline
    const message: Message = isJsonMap(payload) && typeof payload.role === 'string'
      ? (payload as unknown as Message)
      : { role: 'user', content: JSON.stringify(payload) }
    return { ref: sourceRef, digest: row.row.content_digest || digest(payload), messages: [message], observed_at: row.row.observed_at || row.row.created_at, evidence: sourceRef }
  }

  // ── Scope ────────────────────────────────────────────────────────────────

  resolveScope(scope: Scope | undefined, create: boolean): ResolvedScope {
    const contexts = [...new Set(scope?.context_refs ?? [])].sort()
    const requested: Scope = {
      ...(scope?.task_ref === undefined ? {} : { task_ref: scope.task_ref }),
      ...(contexts.length ? { context_refs: contexts } : {}),
    }
    const resolved: ResolvedScope = { requested, task: null, contexts: [] }
    if (requested.task_ref !== undefined) {
      resolved.task = this.scopeConcept(requested.task_ref, create)
      resolved.contexts.push(resolved.task)
    }
    for (const context of contexts) resolved.contexts.push(this.scopeConcept(context, create))
    resolved.contexts = [...new Set(resolved.contexts)].sort()
    return resolved
  }

  private scopeConcept(handle: string, create: boolean): string {
    if (!handle || handle.length > 256 || /\p{Cc}/u.test(handle)) throw new MemoryError('InvalidRequestEnvelope', 'a scope handle is 1..=256 printable characters')
    const id = tryParseElementId(handle)
    if (id) {
      const row = this.nexus.store.load(id)
      if (!row || row.kind !== 'Concept' || row.row.space !== this.nexus.space || row.row.state !== 'active') {
        throw new MemoryError('NotFoundOrNotVisible', 'the scope handle does not name an active Concept of this Space')
      }
      return handle
    }
    const key = `memory_scope:${handle}`
    const found = this.query('FIND(?c.id) WHERE { ?c {type: "Event", key: :key} } LIMIT 1', { key })[0]
    if (typeof found === 'string') return found
    if (!create) return `unbound:${handle}`
    const outcome = this.session.execute(`UPSERT CONCEPT ?scope {
      MATCH {type: "Event", key: :key}
      SET FIELDS {name: :name}
      SET ATTRIBUTES {event_class: "memory_scope", summary: :summary}
    }`, { key, name: handle, summary: `Memory scope handle ${handle}, named by the host` })
    return outcome.handles.scope!
  }

  // ── Intake ───────────────────────────────────────────────────────────────

  record(namespace: string, receiptRef: string): IntakeRecord {
    if (!/^rcpt-[a-f0-9]{40}$/.test(receiptRef)) return notFound('receipt')
    const record = this.kv.get<IntakeRecord>(RECEIPT + receiptRef)
    if (!record || record.namespace !== namespace) return notFound('receipt')
    return record
  }

  /** Current progress; an abandoned pass is failed with an unknown outcome. */
  progress(namespace: string, receiptRef: string): { progress: Progress; record: IntakeRecord } {
    let record = this.record(namespace, receiptRef)
    if (!record.terminal && Date.now() - record.created_at > ABANDONED_MS) {
      record = this.settle(receiptRef, {
        receipt_ref: receiptRef, phase: 'failed', reason: 'the processing request ended without reporting its outcome',
        error: { code: 'OutcomeUnknown', message: 'processing was interrupted; its outcome is unknown and it is not re-run' },
      })
    }
    return { progress: record.terminal ?? { receipt_ref: receiptRef, phase: 'recorded' }, record }
  }

  settle(receiptRef: string, progress: Progress, result?: unknown, warnings: string[] = []): IntakeRecord {
    const record = this.kv.get<IntakeRecord>(RECEIPT + receiptRef) ?? notFound('receipt')
    if (record.terminal) return record
    record.terminal = progress
    if (result !== undefined) record.result = result
    for (const warning of warnings) if (!record.warnings.includes(warning)) record.warnings.push(warning)
    this.kv.put(RECEIPT + receiptRef, record)
    return record
  }

  /** Binds a mutation's key to a receipt, or replays the one it is bound to. */
  admit(namespace: string, space: string, request: MemoryRequest): Admission {
    const operation = request.operation
    const input = request.input
    const receiptRef = `rcpt-${hexId([namespace, space, operation, request.idempotency_key])}`
    const requested = this.resolveScopeShape(request.scope)
    const existing = this.kv.get<IntakeRecord>(RECEIPT + receiptRef)
    let source: ReturnType<MemoryLedger['resolveSource']> | undefined
    let sourceError: unknown
    if (operation !== 'forget') {
      try { source = this.resolveSource(namespace, input.source_ref as string) } catch (error) { sourceError = error }
    }
    const meaning = (sourceDigest: string | undefined) => digest({
      operation, scope: requested, input, source: operation === 'forget' ? null : { ref: input.source_ref, digest: sourceDigest ?? null },
    })
    if (existing) {
      if (existing.namespace !== namespace) notFound('receipt')
      if (existing.intent_digest !== meaning(existing.source_digest) || (source && source.digest !== existing.source_digest)) {
        throw new MemoryError('IdempotencyConflict', 'this idempotency key was used for a different request')
      }
      return { replay: this.progress(namespace, receiptRef).record }
    }
    if (sourceError) throw sourceError
    const scope = this.resolveScope(request.scope, true)
    for (const predecessor of source?.order?.predecessor_receipts ?? []) {
      const { progress } = this.progress(namespace, predecessor)
      if (progress.phase === 'failed') {
        throw new MemoryError('PreconditionFailed', `predecessor receipt ${predecessor} failed, so this revision is not formed`)
      }
    }
    const record: IntakeRecord = {
      receipt: { receipt_ref: receiptRef, operation, space_id: space, accepted_seq: this.seq() },
      namespace, intent_digest: meaning(source?.digest), scope,
      ...(source ? { source_ref: source.ref, source_digest: source.digest } : {}),
      warnings: [], created_at: Date.now(),
    }
    let intent: MemoryIntent | undefined
    if (operation === 'observe' || operation === 'revise') {
      intent = { receipt_ref: receiptRef, operation, source_ref: source!.ref, scope }
      if (operation === 'revise') {
        const changeKind = (input.change_kind ?? 'unspecified') as ChangeKind
        intent.change_kind = changeKind
        if (typeof input.target_ref === 'string') {
          const target = tryParseElementId(input.target_ref)
          const row = target ? this.nexus.store.load(target) : null
          if (!row || row.row.space !== this.nexus.space || row.row.state !== 'active') notFound('revise target')
          intent.target_ref = input.target_ref
          if (changeKind === 'misrecorded') {
            if (row!.kind !== 'Assertion') throw new MemoryError('ConstraintViolation', 'a misrecording repairs an Assertion')
            const original = row!.row.evidence_refs[0]?.id
            if (!original) throw new MemoryError('PreconditionFailed', 'the extraction cites no source to recover the claim from')
            intent.original_evidence = original
            const evidence = this.nexus.store.load(parseElementId(original))
            if (evidence?.kind === 'Evidence') intent.original_observed_at = evidence.row.observed_at
          }
        }
      }
      record.intent = intent
    }
    this.kv.put(RECEIPT + receiptRef, record)
    return {
      record,
      messages: source?.messages ?? [],
      observed_at: source?.observed_at ?? new Date().toISOString(),
      identity: { key: `memory-source:${source?.ref ?? receiptRef}`, parents: source ? [`memory-source-digest:${source.digest}`] : [] },
      ...(intent ? { intent } : {}),
    }
  }

  private resolveScopeShape(scope: Scope | undefined): Scope {
    const contexts = [...new Set(scope?.context_refs ?? [])].sort()
    return {
      ...(scope?.task_ref === undefined ? {} : { task_ref: scope.task_ref }),
      ...(contexts.length ? { context_refs: contexts } : {}),
    }
  }

  /** Settles a Formation pass: its disposition, and a misrecording's repair. */
  finishFormation(receiptRef: string, trace: PassTrace): IntakeRecord {
    const record = this.kv.get<IntakeRecord>(RECEIPT + receiptRef) ?? notFound('receipt')
    if (trace.evidence.length) {
      record.evidence = [...new Set([...(record.evidence ?? []), ...trace.evidence])]
      this.kv.put(RECEIPT + receiptRef, record)
    }
    const warnings: string[] = []
    let refs = trace.formed.slice(0, MAX_ITEMS)
    let disposition: Disposition = trace.formed.length ? 'formed' : trace.evidence.length ? 'evidence_only' : 'skipped'
    let resolved = Math.max(trace.max_seq ?? record.receipt.accepted_seq, record.receipt.accepted_seq)
    const intent = record.intent
    if (intent?.change_kind === 'misrecorded') {
      if (!intent.target_ref || !intent.original_evidence) {
        return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'failed', reason: 'a misrecording repair names its target extraction',
          error: { code: 'PreconditionFailed', message: 'a misrecording repair names its target extraction' } })
      }
      try {
        const target = this.nexus.store.load(parseElementId(intent.target_ref))
        const source = this.nexus.store.load(parseElementId(intent.original_evidence))
        if (target?.kind !== 'Assertion' || source?.kind !== 'Evidence') notFound('repair target')
        const replacements = trace.assertions.filter(id => {
          const row = this.nexus.store.load(parseElementId(id))
          return row?.kind === 'Assertion' && row.row.state === 'active' && row.row.evidence_refs.some(ref => ref.id === intent.original_evidence)
        })
        const evidence = source!.kind === 'Evidence' ? source!.row : notFound('repair source')
        const payload = evidence.payload_inline
        const locator = isJsonMap(payload) && 'content' in payload ? '/content'
          : `bytes=0-${Math.max(0, (typeof payload === 'string' ? payload : JSON.stringify(payload)).length - 1)}`
        const result = this.session.repairRecording({
          source_ref: intent.original_evidence,
          source_digest: evidence.content_digest || digest(payload),
          source_locator: locator,
          invalidated_refs: [intent.target_ref],
          replacement_refs: replacements,
          reason: 'extraction_error',
          expected_versions: { [intent.target_ref]: target!.row.version },
        })
        refs = [String(result.repair_ref), ...refs]
        disposition = 'formed'
        resolved = Math.max(resolved, this.seq())
      } catch (error) {
        const known = error as { code?: string; message?: string }
        return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'failed', reason: 'recording repair was refused',
          error: { code: typeof known.code === 'string' ? known.code : 'InternalError', message: known.message ?? String(error) } })
      }
    } else if (intent?.operation === 'revise' && (intent.change_kind ?? 'unspecified') === 'unspecified') {
      warnings.push('change_kind was unspecified: the revision was recorded as new claims; no earlier claim was superseded or retracted')
    }
    if (disposition === 'skipped') warnings.push('formation found nothing to remember in this source; it was skipped, not learned')
    const summary = disposition === 'formed' ? `Formed ${refs.length} memory element(s) from the source.`
      : disposition === 'evidence_only' ? 'Preserved the source as Evidence; no claim was formed from it.'
        : 'Processed the source; nothing was formed from it.'
    return this.settle(receiptRef, {
      receipt_ref: receiptRef, phase: 'available', disposition, resolved_seq: resolved, available_seq: resolved,
    }, { summary, memory_refs: refs }, warnings)
  }

  /** A pass that failed after intake: a terminal failure, never a retry. */
  failFormation(receiptRef: string, error: { code: string; message: string }): IntakeRecord {
    return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'failed', reason: error.message, error })
  }

  /**
   * feedback, and a misrecording without a target: the source as attributed
   * Evidence, classed by the role the host captured — never an Outcome.
   */
  captureEvidence(receiptRef: string, messages: Message[], observedAt: string, purpose: 'feedback' | 'revise-report', about?: JsonMap): IntakeRecord {
    const record = this.kv.get<IntakeRecord>(RECEIPT + receiptRef) ?? notFound('receipt')
    for (const reference of [about?.decision_ref, about?.attempt_ref]) {
      if (reference === undefined || reference === null) continue
      const id = typeof reference === 'string' ? tryParseElementId(reference) : null
      const row = id ? this.nexus.store.load(id) : null
      if (!row || row.row.space !== this.nexus.space || row.row.state === 'purged') {
        this.kv.delete(RECEIPT + receiptRef)
        notFound('feedback reference')
      }
    }
    const evidence: string[] = []
    let max = record.receipt.accepted_seq
    if (record.source_ref && !record.source_ref.startsWith('src-')) {
      evidence.push(record.source_ref)
    } else {
      messages.forEach((message, index) => {
        const payload = { ...(message as unknown as JsonMap), ...(about ? { memory_feedback: about } : {}) } as JsonMap
        const at = typeof message.timestamp === 'number' ? new Date(message.timestamp).toISOString() : observedAt
        const outcome = this.session.execute(`MUTATE {
          CREATE EVIDENCE ?e { CLIENT KEY :key SET FIELDS { evidence_class: :class, payload: :payload, observed_at: :at } }
        }`, { key: `memory-${purpose}:${receiptRef}:${index + 1}`, class: evidenceClass(message.role), payload, at })
        if (outcome.handles.e) evidence.push(outcome.handles.e)
        if (typeof outcome.space_seq === 'number') max = Math.max(max, outcome.space_seq)
      })
    }
    const warnings = purpose === 'revise-report'
      ? ['partial: a misrecording names the extraction to repair in target_ref; the report was preserved as Evidence and nothing was repaired']
      : []
    return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'available', disposition: 'evidence_only', resolved_seq: max, available_seq: max },
      { summary: purpose === 'feedback' ? `Preserved the feedback as ${evidence.length} attributed Evidence; it is not a grade.` : `Preserved the source as ${evidence.length} Evidence.`, memory_refs: evidence },
      warnings)
  }

  receiptView(namespace: string, receiptRef: string): JsonMap {
    const { progress, record } = this.progress(namespace, receiptRef)
    return { receipt: record.receipt, progress, result: (record.result ?? null) as Json, warnings: record.warnings } as unknown as JsonMap
  }

  // ── forget ───────────────────────────────────────────────────────────────

  /**
   * An ErasurePlan over the target (MI §4): a claim goes through the product
   * deletion path (its tuple, Assertions, Evidence and dependents, with the
   * sources suppressed); uncited Evidence is purged directly; staged bytes are
   * cleared. `completed` only after the engine validated the plan. This
   * Worker's only storage surface is the Durable Object.
   */
  forget(namespace: string, space: string, request: MemoryRequest, owner: boolean): IntakeRecord {
    const input = request.input as { target_ref: string; mode: 'payload_only' | 'semantic' }
    if (input.mode === 'semantic' && !owner) throw new MemoryError('NotAuthorized', "a semantic forget is the owner's decision")
    const receiptRef = `rcpt-${hexId([namespace, space, 'forget', request.idempotency_key])}`
    const meaning = digest({ operation: 'forget', input })
    const existing = this.kv.get<IntakeRecord>(RECEIPT + receiptRef)
    if (existing) {
      if (existing.namespace !== namespace || existing.intent_digest !== meaning) throw new MemoryError('IdempotencyConflict', 'this idempotency key was used for a different request')
      return existing
    }
    const record: IntakeRecord = {
      receipt: { receipt_ref: receiptRef, operation: 'forget', space_id: space, accepted_seq: this.seq() },
      namespace, intent_digest: meaning, scope: { requested: {}, task: null, contexts: [] }, warnings: [], created_at: Date.now(),
    }
    this.kv.put(RECEIPT + receiptRef, record)
    const targets: JsonMap[] = [], roots: string[] = [], host: JsonMap[] = []
    const suppressed = new Set<string>()
    let status: 'completed' | 'partial' | 'blocked' = 'completed'
    let summary = ''
    const evidenceTargets: string[] = []
    let staged: StagedSource | undefined
    try {
      if (input.target_ref.startsWith('src-')) {
        staged = this.stagedSource(namespace, input.target_ref)
        suppressed.add(`memory-source:${staged.source_ref}`)
        suppressed.add(`memory-source-digest:${staged.source_digest}`)
        for (const [, intake] of this.kv.list<IntakeRecord>({ prefix: RECEIPT })) {
          if (intake.source_ref !== staged.source_ref) continue
          for (const reference of (isJsonMap(intake.result) && Array.isArray(intake.result.memory_refs) ? intake.result.memory_refs : [])) {
            if (typeof reference === 'string' && reference.startsWith('E-')) evidenceTargets.push(reference)
          }
          for (const id of intake.evidence ?? []) evidenceTargets.push(id)
        }
      } else {
        const id = tryParseElementId(input.target_ref)
        const row = id ? this.nexus.store.load(id) : null
        if (!row || row.row.space !== this.nexus.space || row.row.state === 'purged') notFound('forget target')
        if (row!.kind === 'Evidence') evidenceTargets.push(input.target_ref)
      }
      if (input.mode === 'payload_only') {
        if (!evidenceTargets.length && !staged) throw new MemoryError('ConstraintViolation', 'payload_only forgets an Evidence payload or a staged source')
        for (const evidence of evidenceTargets) {
          this.session.execute('PURGE PAYLOAD :id CONFIRM "PURGE"', { id: evidence })
          targets.push({ ref: evidence, surface: 'payload', state: 'erased' })
        }
      } else {
        const claims = new Set<string>()
        if (input.target_ref.startsWith('A-')) claims.add(input.target_ref)
        if (input.target_ref.startsWith('P-')) {
          for (const row of this.query('FIND(?a.id) WHERE { ?p PROPOSITION (id: :id) ?a ASSERTION {proposition: ?p} } LIMIT 128', { id: input.target_ref })) {
            if (typeof row === 'string') claims.add(row)
          }
        }
        for (const evidence of evidenceTargets) {
          for (const ref of this.nexus.store.referrers(this.nexus.space, parseElementId(evidence))) {
            const id = ref.from
            if (id.kind === 'Assertion') claims.add(`A-${id.seq}`)
          }
        }
        const erased = new Set<string>()
        for (const claim of [...claims].sort()) {
          if (erased.has(claim)) continue
          try {
            const auth = this.session.auth
            const record = this.product.record(auth, claim)
            const operation = `mi-${receiptRef.slice(5, 21)}-${claim.replace('-', '')}`
            const prepared = this.product.prepare(auth, { operation_id: operation, record_id: claim, expected_revision: record.revision, kind: 'delete' })
            const confirmed = this.product.commit(auth, operation, prepared.preview_digest)
            if (confirmed.state !== 'confirmed') throw new Error(confirmed.error ?? 'memory change did not confirm')
            for (const target of confirmed.preview.targets) {
              if (target.kind === 'evidence') roots.push(target.id)
              targets.push({ ref: target.id, surface: 'element', state: 'erased' })
              erased.add(target.id)
            }
            for (const key of confirmed.preview.excluded_sources) suppressed.add(key)
          } catch (error) {
            const message = error instanceof Error ? error.message : String(error)
            if (message === 'unsupported_scope') {
              status = 'partial'
              summary = `${claim}'s sources cannot be verified, so it was not erased`
              targets.push({ ref: claim, surface: 'element', state: 'pending' })
            } else throw error
          }
        }
        const direct = evidenceTargets.filter(id => !erased.has(id))
        if (!input.target_ref.startsWith('src-') && !erased.has(input.target_ref) && !claims.has(input.target_ref)) direct.push(input.target_ref)
        for (const id of [...new Set(direct)]) {
          if (id.startsWith('E-')) roots.push(id)
          try {
            const outcome = this.session.execute('PURGE :id REFERENCE POLICY "authorized_cascade" CONFIRM "PURGE"', { id })
            for (const change of outcome.changes) if (change.op === 'purge') targets.push({ ref: change.id, surface: 'element', state: 'erased' })
          } catch (error) {
            if ((error as { code?: string }).code === 'LegalHoldConflict') { status = 'blocked'; summary = `a legal hold retains ${id}`; break }
            throw error
          }
          if (!id.startsWith('E-') && roots.length === 0 && status === 'completed') {
            status = 'partial'
            summary = `${id} was purged, but the sources it was formed from were not enumerated, so erasure of their copies is not verified`
          }
        }
        if (suppressed.size) this.product.suppress([...suppressed])
      }
      for (const key of suppressed) {
        const ref = key.startsWith('memory-source:') ? key.slice('memory-source:'.length) : null
        if (ref && this.eraseStaged(ref)) host.push({ surface: 'blob', ref, state: 'erased' })
      }
      if (staged && !suppressed.has(`memory-source:${staged.source_ref}`) && this.eraseStaged(staged.source_ref)) {
        host.push({ surface: 'blob', ref: staged.source_ref, state: 'erased' })
      }
      if (staged) this.product.suppress([...suppressed])
    } catch (error) {
      const known = error as { code?: string; message?: string }
      return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'failed', reason: 'the erasure could not run',
        error: { code: typeof known.code === 'string' ? known.code : 'InternalError', message: known.message ?? String(error) } })
    }
    const seq = this.seq()
    const plan: JsonMap = {
      scope: input.mode === 'semantic' ? 'semantic_forgetting' : 'payload_only',
      basis_seq: seq, source_event_refs: [...new Set(roots)].sort(), targets: targets as unknown as Json[],
      external_exports: this.deliveredBases(targets.map(t => String(t.ref))), status, receipts: [receiptRef],
    }
    if (status === 'completed') {
      try { this.session.validateErasurePlan(plan) } catch (error) {
        status = 'partial'
        summary = `the erasure could not be verified complete: ${error instanceof Error ? error.message : String(error)}`
        plan.status = 'partial'
      }
    }
    if (!summary) summary = `Erased ${targets.length} element(s) or payload(s) and ${host.length} host copy surface(s).`
    const planRef = `plan-${receiptRef.slice(5)}`
    this.kv.put(PLAN + planRef, { plan_ref: planRef, namespace, receipt_ref: receiptRef, plan, host_surfaces: host })
    const result = { status, plan_ref: planRef, summary, coverage_ref: planRef }
    if (status === 'completed') {
      return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'available', disposition: 'erased', resolved_seq: seq, available_seq: seq }, result)
    }
    if (status === 'blocked') {
      return this.settle(receiptRef, { receipt_ref: receiptRef, phase: 'failed', reason: summary, error: { code: 'LegalHoldConflict', message: summary } }, result)
    }
    record.result = result
    record.warnings = [`partial: ${summary}`]
    this.kv.put(RECEIPT + receiptRef, record)
    return record
  }

  plan(namespace: string, planRef: string): JsonMap {
    const retained = this.kv.get<JsonMap & { namespace: string }>(PLAN + planRef)
    if (!retained || retained.namespace !== namespace) return notFound('plan')
    const { namespace: _owner, ...view } = retained
    return view as JsonMap
  }

  private eraseStaged(sourceRef: string): boolean {
    const staged = this.kv.get<StagedSource>(SOURCE + sourceRef)
    if (!staged || staged.erased) return false
    staged.messages = []
    staged.erased = true
    this.kv.put(SOURCE + sourceRef, staged)
    return true
  }

  private deliveredBases(erased: string[]): string[] {
    const set = new Set(erased)
    const found: string[] = []
    for (const [key, value] of this.kv.list<RetainedRecall>({ prefix: RECALL })) {
      if (found.length >= 128) break
      if (value.items.some(([, pins]) => pins.some(([id]) => set.has(id)))) found.push(key.slice(RECALL.length))
    }
    return found
  }

  // ── recall ───────────────────────────────────────────────────────────────

  /** Waits nothing: a Worker pass settles within its request. */
  barrier(namespace: string, after: string[]): Progress[] {
    return [...new Set(after)].map(receipt => this.progress(namespace, receipt).progress)
  }

  /**
   * Builds a briefing from what the Recall pass cited plus the host's own
   * channel reads (MI §6; see the Rust `recall.rs`). Returns the briefing
   * without its budget applied; `memory.ts` trims optional items to fit.
   */
  briefing(
    _namespace: string, scopeInput: Scope | undefined, cited: string[], options: {
      mode: string; valid_at?: string; as_of_seq?: number; query?: string; after: Progress[]
      uncertainties: string[]; evidence_complete: boolean; summary?: string
    },
  ): { briefing: Briefing; candidates: Candidate[]; dropped: boolean } {
    const scope = this.resolveScope(scopeInput, false)
    const snapshot = options.as_of_seq ?? this.seq()
    const candidates: Candidate[] = []
    let dropped = false
    for (const id of cited) {
      const candidate = this.citedItem(id, scope, options.valid_at, options.as_of_seq)
      if (candidate === null) continue
      if (candidate === 'out_of_scope') { dropped = true; continue }
      if (!candidates.some(c => c.pins[0]?.[0] === candidate.pins[0]?.[0])) candidates.push(candidate)
    }
    const plans: Record<string, Plan> = {}
    const exact = (name: string, command: string) => {
      const [items, plan] = this.exactChannel(command, scope, options.as_of_seq)
      plans[name] = plan
      for (const item of items) if (!candidates.some(c => c.pins[0]?.[0] === item.pins[0]?.[0])) candidates.push(item)
    }
    exact('constraints', 'FIND(?c) WHERE { ?c {type: "Insight"} FILTER(?c.attributes.insight_class == "constraint") }')
    exact('commitments', 'FIND(?c) WHERE { ?c {type: "Commitment"} FILTER(IN(?c.attributes.status, ["pending", "blocked"])) }')
    const search = (name: string, type: string, filter: string) => {
      const [items, plan] = this.searchChannel(options.query ?? '', type, filter, scope, options.as_of_seq)
      plans[name] = plan
      for (const item of items) if (!candidates.some(c => c.pins[0]?.[0] === item.pins[0]?.[0])) candidates.push(item)
    }
    search('failures', 'Experience', ' FILTER(IN(?c.attributes.outcome_status, ["failure", "aborted"]))')
    search('experiences', 'Experience', ' FILTER(IN(?c.attributes.outcome_status, ["success", "partial", "unknown"]))')
    search('skills', 'Skill', '')
    plans.dependencies = { exact: true, complete: true, truncation: null }
    plans.evidence = { exact: false, complete: options.evidence_complete, truncation: options.evidence_complete ? null : 'unsupported' }
    const unverified: string[] = []
    for (const candidate of [...candidates]) {
      const row = this.row(candidate.pins[0]![0], options.as_of_seq)
      const validity = isJsonMap(row) && isJsonMap(row._system) ? row._system.dependency_validity : undefined
      const status = isJsonMap(validity) ? validity.status : validity
      if (status === 'needs_review' || status === 'unverifiable') {
        const note = `${candidate.pins[0]![0]} depends on memory that changed (${status}); review before relying on it`
        unverified.push(note)
        candidates.push({ item: { ref: '', text: note, role: 'warning', epistemic_status: 'uncertain', evidence_refs: [candidate.pins[0]![0]], action_eligible: false },
          pins: candidate.pins, required: true })
      }
    }
    if (options.mode === 'action') {
      for (const candidate of candidates) {
        if ((candidate.item.epistemic_status === 'contested' || candidate.item.epistemic_status === 'uncertain') &&
            (candidate.item.role === 'fact' || candidate.item.role === 'constraint')) {
          unverified.push(`${candidate.pins[0]?.[0] ?? 'an item'} is ${candidate.item.epistemic_status}, not accepted`)
        }
      }
    }
    const uncertainties = [...options.uncertainties]
    if (candidates.length === 0 && options.query) uncertainties.push('no recorded memory answers this; the basis is insufficient, which is not a no')
    candidates.sort((a, b) => Number(b.required) - Number(a.required) || roleRank(a.item.role) - roleRank(b.item.role))
    candidates.splice(MAX_ITEMS)
    const summary = options.summary && !dropped ? options.summary
      : candidates.length ? candidates.slice(0, 12).map(c => `- ${c.item.text}`).join('\n') : 'No recorded memory bears on this in scope.'
    const briefing = this.assemble(scope, snapshot, plans, candidates, options.after, unverified, uncertainties, summary)
    return { briefing, candidates, dropped }
  }

  /** The briefing for a candidate set, with fresh item refs. */
  assemble(scope: ResolvedScope, snapshot: number, plans: Record<string, Plan>, candidates: Candidate[], after: Progress[],
    unverified: string[], uncertainties: string[], summary: string): Briefing {
    const channels = Object.fromEntries(CHANNELS.map(name => [name, plans[name]?.complete === false ? 'incomplete' : 'complete'])) as unknown as Channels
    const pending = after.filter(p => p.phase !== 'available' || (p.available_seq ?? Infinity) > snapshot).map(p => p.receipt_ref)
    const coverage = coverageOf(scope.requested, channels, pending, [...new Set(unverified)])
    const basis = `basis-${hexId([crypto.randomUUID()])}`
    const items = candidates.map((candidate, index): MemoryItem => ({
      ...candidate.item, ref: `${basis}:${index}`,
      action_eligible: coverage.action_eligible && candidate.item.epistemic_status === 'accepted' && (candidate.item.role === 'fact' || candidate.item.role === 'constraint'),
    }))
    const briefing: Briefing = { summary: summary.slice(0, 4096) || 'No recorded memory bears on this in scope.', items,
      uncertainties: [...new Set(uncertainties)].slice(0, 128), basis_ref: basis, coverage, after }
    ;(briefing as Briefing & { __plans?: Record<string, Plan>; __snapshot?: number; __scope?: ResolvedScope }).__plans = plans
    return briefing
  }

  /**
   * The whole recall answer: builds the briefing, trims optional items to the
   * output budget (required constraints and warnings never go; what does not
   * fit makes its channel incomplete), retains the basis and logs exposure.
   */
  deliver(
    namespace: string, scopeInput: Scope | undefined, cited: string[], options: {
      mode: string; valid_at?: string; as_of_seq?: number; query?: string; after: Progress[]
      uncertainties: string[]; evidence_complete: boolean; summary?: string; max_tokens: number
      attention?: AttentionItem[]; attention_cursor?: string; warnings: string[]
    },
  ): Briefing {
    const scope = this.resolveScope(scopeInput, false)
    const snapshot = options.as_of_seq ?? this.seq()
    const built = options.mode === 'attention'
      ? { briefing: this.assemble(scope, snapshot, Object.fromEntries(CHANNELS.map(name => [name, { exact: true, complete: true, truncation: null }])),
          [], options.after, [], options.uncertainties, `${options.attention?.length ?? 0} attention item(s) raised since the cursor. An item is a prompt to think, never permission to act.`),
        candidates: [] as Candidate[], dropped: false }
      : this.briefing(namespace, scopeInput, cited, options)
    if (built.dropped) options.warnings.push('the recall pass cited memory outside this scope; it was left out and the summary lists only in-scope items')
    const plans = (built.briefing as Briefing & { __plans?: Record<string, Plan> }).__plans ?? {}
    const candidates = built.candidates
    for (;;) {
      const briefing = this.assemble(scope, snapshot, plans, candidates, options.after,
        built.briefing.coverage.unverified_preconditions, built.briefing.uncertainties, built.briefing.summary)
      if (options.mode === 'attention') {
        for (const name of CHANNELS) (briefing.coverage.channels as unknown as Record<string, ChannelState>)[name] = 'not_applicable'
      }
      if (options.attention) {
        briefing.attention = options.attention
        if (options.attention_cursor) briefing.attention_cursor = options.attention_cursor
      }
      delete (briefing as Briefing & { __plans?: unknown }).__plans
      if (new TextEncoder().encode(JSON.stringify(briefing)).byteLength <= options.max_tokens) {
        this.retain(namespace, scopeInput, briefing, candidates, plans, snapshot)
        return briefing
      }
      let index = -1
      for (let i = candidates.length - 1; i >= 0; i -= 1) if (!candidates[i]!.required) { index = i; break }
      if (index < 0) {
        throw new MemoryError('ResultLimitExceeded', `the required constraints, warnings and coverage need more than ${options.max_tokens} tokens`)
      }
      const [removed] = candidates.splice(index, 1)
      const channel = removed!.item.role === 'experience' ? 'experiences' : removed!.item.role === 'procedure' ? 'skills' : 'evidence'
      plans[channel] = { ...(plans[channel] ?? { exact: false }), complete: false, truncation: 'budget' }
    }
  }

  /** Retains a delivered briefing's basis and records `retrieved` exposure. */
  retain(namespace: string, scopeInput: Scope | undefined, briefing: Briefing, candidates: Candidate[], plans: Record<string, Plan>, snapshot: number): void {
    const scope = this.resolveScope(scopeInput, false)
    const basisRow = this.basis(candidates, scope, snapshot)
    const authorizationView = isJsonMap(basisRow) && typeof basisRow.authorization_view === 'string' ? basisRow.authorization_view : 'kip:system'
    const channelStates: JsonMap = {}, planValues: JsonMap = {}
    for (const name of CHANNELS) {
      const plan = plans[name] ?? { exact: false, complete: true, truncation: null }
      const state = (briefing.coverage.channels as unknown as Record<string, ChannelState>)[name]
      channelStates[name] = { completed: state !== 'incomplete', truncated: plan.truncation !== null }
      planValues[name] = {
        selector: { artifact_ref: `anda-brain:recall-plan/${name}`, content_digest: digest({ channel: name, exact: plan.exact }) },
        scope: { task_ref: scope.task, context_refs: scope.contexts },
        method: plan.exact ? 'exact' : 'approximate', snapshot_seq: snapshot, index_seq: snapshot, covered_through_seq: snapshot,
        authorization_view: authorizationView, complete: plan.complete, truncation_reason: plan.truncation,
      }
    }
    const retained: RetainedRecall = {
      namespace, snapshot_seq: snapshot, scope,
      basis: basisRow ?? null,
      coverage: { basis: basisRow ?? null, channels: channelStates, unverified_preconditions: briefing.coverage.unverified_preconditions,
        action_eligible: briefing.coverage.action_eligible, plans: planValues },
      items: briefing.items.map((item, index) => [item.ref, candidates[index]?.pins ?? []]),
      created_at: Date.now(),
    }
    this.kv.put(RECALL + briefing.basis_ref, retained)
    const entries = briefing.items.flatMap((_, index) => (candidates[index]?.pins ?? []).map(([id]) => ({
      element_id: id, exposure: 'retrieved' as const, snapshot_seq: Math.min(snapshot, this.seq()), recall_ref: briefing.basis_ref,
    }))).slice(0, 256)
    if (entries.length) {
      try { this.session.recordExposures(entries) } catch (error) { console.warn('exposure log write failed', error) }
    }
  }

  /** `detail: "evidence"`: the retained basis and pinned versions (MI §6). */
  expand(namespace: string, target: string, evidence: boolean): Briefing {
    const [basisId, index] = target.includes(':') ? target.split(':', 2) as [string, string] : [target, undefined]
    if (!/^basis-[a-f0-9]{40}$/.test(basisId)) notFound('result reference')
    const retained = this.kv.get<RetainedRecall>(RECALL + basisId)
    if (!retained || retained.namespace !== namespace) return notFound('result reference')
    const selected = retained.items.filter(([ref]) => index === undefined || ref === target)
    if (index !== undefined && selected.length === 0) notFound('result reference')
    const elements: Json[] = []
    const uncertainties: string[] = []
    for (const [, pins] of selected) {
      for (const [id, version] of pins) {
        const row = this.row(id, retained.snapshot_seq)
        const system = isJsonMap(row) && isJsonMap(row._system) ? row._system : undefined
        if (row && system && system.state !== 'purged' && (version === 0 || system.version === version)) {
          if (elements.length < MAX_ITEMS) elements.push(row)
        } else uncertainties.push(`${id} as it was at this result is no longer available`)
      }
    }
    const all = Object.fromEntries(CHANNELS.map(name => [name, 'complete'])) as unknown as Channels
    return {
      summary: `Expansion of ${target} at snapshot ${retained.snapshot_seq}: ${elements.length} element(s) retained.`,
      items: [], uncertainties, basis_ref: basisId,
      coverage: { ...coverageOf(retained.scope.requested, all, [], []), action_eligible: false },
      after: [],
      ...(evidence ? { details: { basis: retained.basis, coverage: retained.coverage, elements } } : {}),
    }
  }

  /** Attention the scope may see: a target formed in another task is not this task's. */
  scopedAttention(scopeInput: Scope | undefined, items: { ref: string; kind: string; summary: string; raised_seq: number; due_at?: string; target_refs: string[]; priority?: number }[]): AttentionItem[] {
    const scope = this.resolveScope(scopeInput, false)
    return items.filter(item => item.target_refs.every(target => {
      const row = this.row(target)
      const [task, contexts] = conceptScope(row)
      return admits(scope, task, contexts)
    })).map(item => ({
      ref: item.ref, kind: item.kind === 'commitment_due' ? 'commitment_due' : 'watch_fired', summary: item.summary || 'attention raised',
      raised_seq: item.raised_seq, ...(item.due_at ? { due_at: item.due_at } : {}), target_refs: item.target_refs,
      ...(item.priority === undefined ? {} : { priority: item.priority }),
    }))
  }

  // ── helpers ──────────────────────────────────────────────────────────────

  private row(id: string, asOf?: number): JsonMap | null {
    const parsed = tryParseElementId(id)
    if (!parsed) return null
    const pattern = parsed.kind === 'Concept' ? '?e CONCEPT {id: :id}'
      : parsed.kind === 'Proposition' ? '?e PROPOSITION (id: :id)'
        : parsed.kind === 'Assertion' ? '?e ASSERTION {id: :id}'
          : parsed.kind === 'Evidence' ? '?e EVIDENCE {id: :id}' : '?e ACTIVITY {id: :id}'
    try {
      const rows = this.query(`FIND(?e) WHERE { ${pattern} }${asOf === undefined ? '' : ` AS OF SEQ ${asOf}`} LIMIT 1`, { id })
      return isJsonMap(rows[0]) ? rows[0] : null
    } catch { return null }
  }

  /** An endpoint as a reader sees it: a Concept's name, else the literal. */
  private label(endpoint: Json | undefined): string {
    const id = isJsonMap(endpoint) && typeof endpoint.id === 'string' ? tryParseElementId(endpoint.id) : null
    const row = id ? this.nexus.store.load(id) : null
    if (row?.kind === 'Concept') return row.row.name.slice(0, 512)
    return JSON.stringify(endpoint ?? null).slice(0, 512)
  }

  private belief(proposition: string, scope: ResolvedScope, validAt?: string, asOf?: number): JsonMap | null {
    const params: JsonMap = { id: proposition, contexts: scope.contexts }
    let command = 'FIND(?b) WHERE { ?b BELIEF (id: :id) }'
    if (asOf !== undefined) command += ` AS OF SEQ ${asOf}`
    if (validAt) { command += ' FOR TIME :valid_at'; params.valid_at = validAt }
    command += ' WITH EPISTEMIC {purpose: "memory_recall", risk: "low", context_refs: :contexts}'
    try {
      const rows = this.query(command, params)
      return isJsonMap(rows[0]) ? rows[0] : null
    } catch { return null }
  }

  private basis(candidates: Candidate[], scope: ResolvedScope, snapshot: number): JsonMap | null {
    for (const candidate of candidates) {
      for (const [id] of candidate.pins) {
        if (id.startsWith('P-')) {
          const belief = this.belief(id, scope, undefined, snapshot)
          if (belief && isJsonMap(belief.basis)) return belief.basis
        }
      }
    }
    try {
      const rows = this.query(`FIND(?b) WHERE { ?s {type: "Event", key: "memory_scope:__basis__"} ?b BELIEF (?s, "same_as", ?s) } AS OF SEQ ${snapshot}`)
      return isJsonMap(rows[0]) && isJsonMap(rows[0].basis) ? rows[0].basis : null
    } catch { return null }
  }

  private citedItem(id: string, scope: ResolvedScope, validAt?: string, asOf?: number): Candidate | 'out_of_scope' | null {
    const parsed = tryParseElementId(id)
    if (!parsed) return null
    const row = this.row(id, asOf)
    const system = row && isJsonMap(row._system) ? row._system : null
    if (!row || !system || system.state !== 'active') return null
    const version = typeof system.version === 'number' ? system.version : 0
    if (parsed.kind === 'Assertion') {
      if (isJsonMap(system.recording_validity) && system.recording_validity.status === 'invalidated') return null
      const contexts = (Array.isArray(row.context_refs) ? row.context_refs : []).flatMap(ref =>
        typeof ref === 'string' ? [ref] : isJsonMap(ref) && typeof ref.id === 'string' ? [ref.id] : [])
      if (!admits(scope, null, contexts)) return 'out_of_scope'
      const proposition = typeof row.proposition === 'string' ? row.proposition
        : isJsonMap(row.proposition) && typeof row.proposition.id === 'string' ? row.proposition.id
          : typeof row.proposition_id === 'string' ? row.proposition_id : null
      if (!proposition) return null
      const belief = this.belief(proposition, scope, validAt, asOf)
      const prop = this.row(proposition, asOf)
      const text = prop ? `${this.label(prop.subject)} · ${local(String(prop.predicate_ref ?? '?'))} · ${this.label(prop.object)}` : proposition
      const evidence = (Array.isArray(row.evidence) ? row.evidence : Array.isArray(row.evidence_refs) ? row.evidence_refs : [])
        .flatMap(ref => typeof ref === 'string' ? [ref] : isJsonMap(ref) && typeof ref.id === 'string' ? [ref.id] : []).slice(0, 32)
      return { item: { ref: '', text: text.slice(0, 4096), role: 'fact', epistemic_status: statusOf(belief?.status), evidence_refs: evidence, action_eligible: false },
        pins: [[id, version], [proposition, 0]], required: false }
    }
    if (parsed.kind === 'Proposition') {
      // A tuple is content too: it belongs to this scope only when one of its claims does.
      let claims: Json[] = []
      try { claims = this.query('FIND(?a) WHERE { ?p PROPOSITION (id: :id) ?a ASSERTION {proposition: ?p} } LIMIT 64', { id }) } catch { return null }
      const inScope = claims.some(claim => isJsonMap(claim) && admits(scope, null, (Array.isArray(claim.context_refs) ? claim.context_refs : [])
        .flatMap(ref => typeof ref === 'string' ? [ref] : isJsonMap(ref) && typeof ref.id === 'string' ? [ref.id] : [])))
      if (!inScope) return claims.length ? 'out_of_scope' : null
      const belief = this.belief(id, scope, validAt, asOf)
      return { item: { ref: '', text: `${this.label(row.subject)} · ${local(String(row.predicate_ref ?? '?'))} · ${this.label(row.object)}`.slice(0, 4096),
        role: 'fact', epistemic_status: statusOf(belief?.status), evidence_refs: [], action_eligible: false }, pins: [[id, version]], required: false }
    }
    if (parsed.kind === 'Concept') {
      const [task, contexts] = conceptScope(row)
      if (!admits(scope, task, contexts)) return 'out_of_scope'
      return conceptItem(row, false)
    }
    if (parsed.kind === 'Evidence') {
      const [task, contexts] = conceptScope(row)
      if (!admits(scope, task, contexts)) return 'out_of_scope'
      return { item: { ref: '', text: `Source: ${JSON.stringify(row.payload)}`.slice(0, 4096), role: 'source', epistemic_status: 'not_applicable', evidence_refs: [id], action_eligible: false },
        pins: [[id, version]], required: false }
    }
    return null
  }

  private exactChannel(command: string, scope: ResolvedScope, asOf?: number): [Candidate[], Plan] {
    let rows: Json[]
    try { rows = this.query(`${command}${asOf === undefined ? '' : ` AS OF SEQ ${asOf}`} LIMIT ${CHANNEL_LIMIT + 1}`) } catch {
      return [[], { exact: true, complete: false, truncation: 'unsupported' }]
    }
    const truncated = rows.length > CHANNEL_LIMIT
    const items = rows.slice(0, CHANNEL_LIMIT).flatMap(row => {
      if (!isJsonMap(row)) return []
      const [task, contexts] = conceptScope(row)
      if (!admits(scope, task, contexts)) return []
      const candidate = conceptItem(row, true)
      return candidate ? [candidate] : []
    })
    return [items, { exact: true, complete: !truncated, truncation: truncated ? 'page_limit' : null }]
  }

  private searchChannel(query: string, type: string, filter: string, scope: ResolvedScope, asOf?: number): [Candidate[], Plan] {
    let rows: Json[]
    try {
      if (query.trim()) {
        if (asOf !== undefined) return [[], { exact: false, complete: false, truncation: 'unsupported' }]
        rows = this.query(`FIND(?c) WHERE { ?c SEARCH CONCEPT :query WITH TYPE "${type}" LIMIT ${SEARCH_LIMIT * 2}${filter} } LIMIT ${SEARCH_LIMIT}`, { query })
      } else {
        rows = this.query(`FIND(?c) WHERE { ?c {type: "${type}"}${filter} }${asOf === undefined ? '' : ` AS OF SEQ ${asOf}`} LIMIT ${SEARCH_LIMIT + 1}`)
      }
    } catch {
      return [[], { exact: false, complete: false, truncation: 'unsupported' }]
    }
    const truncated = !query.trim() && rows.length > SEARCH_LIMIT
    const items = rows.slice(0, SEARCH_LIMIT).flatMap(row => {
      if (!isJsonMap(row)) return []
      const [task, contexts] = conceptScope(row)
      if (!admits(scope, task, contexts)) return []
      const candidate = conceptItem(row, false)
      return candidate ? [candidate] : []
    })
    return [items, truncated ? { exact: true, complete: false, truncation: 'page_limit' } : { exact: false, complete: true, truncation: null }]
  }
}

export interface Candidate { item: MemoryItem; pins: [string, number][]; required: boolean }
export interface Plan { exact: boolean; complete: boolean; truncation: string | null }
interface RetainedRecall {
  namespace: string; snapshot_seq: number; scope: ResolvedScope; basis: Json; coverage: JsonMap
  items: [string, [string, number][]][]; created_at: number
}

export const CHANNELS = ['constraints', 'commitments', 'dependencies', 'failures', 'experiences', 'skills', 'evidence'] as const

function statusOf(value: unknown): EpistemicStatus {
  return value === 'accepted' || value === 'rejected' || value === 'contested' || value === 'uncertain' ? value : 'insufficient'
}

function roleRank(role: Role): number {
  return ({ constraint: 0, warning: 1, fact: 2, procedure: 3, experience: 4, source: 5 } as const)[role]
}

function conceptScope(row: JsonMap | null): [string | null, string[]] {
  const facets = row && isJsonMap(row.facets) ? row.facets : null
  const scope = facets ? Object.entries(facets).find(([name]) => local(name) === 'MemoryScope')?.[1] : undefined
  if (!isJsonMap(scope)) return [null, []]
  const task = typeof scope.task_ref === 'string' ? scope.task_ref : null
  const contexts = Array.isArray(scope.context_refs) ? scope.context_refs.filter((c): c is string => typeof c === 'string') : []
  return [task, contexts]
}

function admits(scope: ResolvedScope, task: string | null, contexts: string[]): boolean {
  if (task !== null && scope.task !== task) return false
  return contexts.every(context => scope.contexts.includes(context))
}

function conceptItem(row: JsonMap, required: boolean): Candidate | null {
  const id = typeof row.id === 'string' ? row.id : null
  if (!id) return null
  const type = local(typeof row.schema_ref === 'string' ? row.schema_ref : typeof row.type === 'string' ? row.type : '')
  const attributes = isJsonMap(row.attributes) ? row.attributes : {}
  const name = typeof row.name === 'string' ? row.name : ''
  const summary = typeof attributes.summary === 'string' ? attributes.summary : typeof attributes.goal === 'string' ? attributes.goal : name
  const version = isJsonMap(row._system) && typeof row._system.version === 'number' ? row._system.version : 0
  // The host's own scope handles are bookkeeping, not memory.
  if (attributes.event_class === 'memory_scope') return null
  const make = (text: string, role: Role, status: EpistemicStatus, isRequired: boolean, standing?: MemoryItem['standing']): Candidate => ({
    item: { ref: '', text: text.slice(0, 4096), role, epistemic_status: status, evidence_refs: [], action_eligible: false, ...(standing ? { standing } : {}) },
    pins: [[id, version]], required: isRequired,
  })
  if (type === 'Insight' && attributes.insight_class === 'constraint') return make(`Constraint: ${summary}`, 'constraint', 'accepted', true)
  if (type === 'Commitment') {
    return make(`Open commitment (${String(attributes.status ?? 'pending')}): ${summary}${typeof attributes.due_at === 'string' ? `, due ${attributes.due_at}` : ''}`, 'constraint', 'accepted', required)
  }
  if (type === 'Experience' || type === 'Event') {
    const outcome = String(attributes.outcome_status ?? 'unknown')
    const failed = outcome === 'failure' || outcome === 'aborted'
    return make(`${failed ? 'Past failure' : 'Experience'} (${outcome}): ${summary}`, failed ? 'warning' : 'experience', failed ? 'uncertain' : 'not_applicable', required || failed)
  }
  if (type === 'Skill') {
    const revoked = attributes.status === 'revoked' || (isJsonMap(row.lifecycle) && row.lifecycle.status === 'revoked')
    return make(`Procedure candidate (unproven, grants no permission): ${name}`, 'procedure', 'not_applicable', required, revoked ? 'revoked' : 'unproven')
  }
  if (type === 'Insight') return make(`Insight: ${summary}`, 'fact', 'uncertain', required)
  return null
}

