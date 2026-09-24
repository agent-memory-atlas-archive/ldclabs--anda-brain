/**
 * The KIP 2.0 Memory Interface wire (`KIP-2.0-Memory-Interface.md`, shapes in
 * `kip-memory.schema.json`): one intent per request, the envelope rules the
 * schema states, and the response shapes this Worker returns. Mirrors
 * `anda_kip::memory::binding` in the Rust Brain.
 */

export const KIP_MEMORY = '2.0'
export const MAX_AFTER = 128
/** Same pinned name the Rust Brain advertises; see {@link fitsBudget}. */
export const TOKENIZER = 'o200k_base@tiktoken-rs-0.12.0'
export const DEFAULT_OUTPUT_TOKENS = 4096
export const DEFAULT_DEADLINE_MS = 30_000
export const MAX_DEADLINE_MS = 120_000
export const MINIMUM_RESPONSE_TOKENS = 256

export type Operation = 'observe' | 'recall' | 'revise' | 'feedback' | 'forget'
export type Bundle = 'memory_basic' | 'memory_experience' | 'memory_learning'
export type Phase = 'recorded' | 'processed' | 'available' | 'failed'
export type Disposition = 'formed' | 'evidence_only' | 'skipped' | 'erased'
export type Status = 'succeeded' | 'pending' | 'partial' | 'failed'
export type ChangeKind = 'correction' | 'world_change' | 'misrecorded' | 'unspecified'
export type ChannelState = 'complete' | 'incomplete' | 'not_applicable'
export type Role = 'fact' | 'constraint' | 'experience' | 'procedure' | 'warning' | 'source'
export type EpistemicStatus = 'accepted' | 'rejected' | 'contested' | 'uncertain' | 'insufficient' | 'not_applicable'

export interface Scope { task_ref?: string; context_refs?: string[] }
export interface Budget { max_output_tokens?: number; deadline_ms?: number; tokenizer?: string }
export interface MemoryRequest {
  kip_memory: string
  request_id?: string
  operation: Operation
  space?: { id?: string; uri?: string }
  scope?: Scope
  budget?: Budget
  idempotency_key?: string
  requires?: Bundle[]
  input: Record<string, unknown>
}
export interface KipErrorObject { code: string; message: string; category?: string; hint?: string }
export interface Receipt { receipt_ref: string; operation: Operation; space_id: string; accepted_seq: number }
export interface Progress {
  receipt_ref: string; phase: Phase; disposition?: Disposition
  resolved_seq?: number; available_seq?: number; reason?: string; error?: KipErrorObject
}
export interface MemoryItem {
  ref: string; text: string; role: Role; epistemic_status: EpistemicStatus
  evidence_refs: string[]; action_eligible: boolean; standing?: 'unproven' | 'validated' | 'revoked' | 'unverifiable'
}
export interface Channels {
  constraints: ChannelState; commitments: ChannelState; dependencies: ChannelState
  failures: ChannelState; experiences: ChannelState; skills: ChannelState; evidence: ChannelState
}
export interface Coverage {
  complete: boolean; scope: Scope; channels: Channels; pending_receipts: string[]
  unverified_preconditions: string[]; action_eligible: boolean
}
export interface AttentionItem {
  ref: string; kind: 'watch_fired' | 'commitment_due'; summary: string; raised_seq: number
  due_at?: string; target_refs: string[]; priority?: number
}
export interface Briefing {
  summary: string; items: MemoryItem[]; uncertainties: string[]; basis_ref: string
  coverage: Coverage; after: Progress[]; continuation_ref?: string
  details?: { basis: unknown; coverage: unknown; elements?: unknown[] }
  attention?: AttentionItem[]; attention_cursor?: string
}
export interface MemoryResponse {
  kip_memory: string; request_id?: string; operation: Operation; status: Status
  receipt?: Receipt; progress?: Progress; result?: unknown; error?: KipErrorObject; warnings: string[]
}
export interface Descriptor {
  kip_memory: string; bundles: Bundle[]; default_scope?: Scope
  default_budget: { max_output_tokens: number; deadline_ms: number }
  tokenizer: string; minimum_response_tokens: number; default_space?: { id: string }
}

/** A Memory Interface failure carrying its KIP error code. */
export class MemoryError extends Error {
  constructor(readonly code: string, message: string) {
    super(message)
    this.name = 'MemoryError'
  }
  toJSON(): KipErrorObject { return { code: this.code, message: this.message } }
}

export const errorOf = (error: unknown): KipErrorObject => {
  if (error instanceof MemoryError) return error.toJSON()
  const known = error as { code?: unknown; message?: unknown } | null
  if (known && typeof known.code === 'string' && typeof known.message === 'string') {
    return { code: known.code, message: known.message }
  }
  const message = error instanceof Error ? error.message : String(error)
  if (message === 'source_suppressed') return { code: 'NotFoundOrNotVisible', message: 'the source is excluded from memory by an earlier forget' }
  if (message === 'memory_change_pending') return { code: 'PreconditionFailed', message: 'a managed memory change is reconciling; retry after it finishes' }
  return { code: 'InternalError', message }
}

/** What this Worker advertises (MI §2): `memory_basic` only. */
export function descriptor(spaceId?: string): Descriptor {
  return {
    kip_memory: KIP_MEMORY,
    bundles: ['memory_basic'],
    default_budget: { max_output_tokens: DEFAULT_OUTPUT_TOKENS, deadline_ms: DEFAULT_DEADLINE_MS },
    tokenizer: TOKENIZER,
    minimum_response_tokens: MINIMUM_RESPONSE_TOKENS,
    ...(spaceId === undefined ? {} : { default_space: { id: spaceId } }),
  }
}

const OPERATIONS = new Set<Operation>(['observe', 'recall', 'revise', 'feedback', 'forget'])
const BUNDLES = new Set<Bundle>(['memory_basic', 'memory_experience', 'memory_learning'])
const MUTATIONS = new Set<Operation>(['observe', 'revise', 'feedback', 'forget'])
const REQUEST_KEYS = new Set(['kip_memory', 'request_id', 'operation', 'space', 'scope', 'budget', 'idempotency_key', 'requires', 'input'])
const INPUT_KEYS: Record<Operation, Set<string>> = {
  observe: new Set(['source_ref']),
  recall: new Set(['query', 'target_ref', 'mode', 'goal', 'context', 'after', 'detail', 'time', 'attention_cursor']),
  revise: new Set(['source_ref', 'target_ref', 'change_kind']),
  feedback: new Set(['source_ref', 'decision_ref', 'attempt_ref']),
  forget: new Set(['target_ref', 'mode']),
}

const invalid = (message: string): never => { throw new MemoryError('InvalidRequestEnvelope', message) }
const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
const isRef = (value: unknown): value is string =>
  typeof value === 'string' && value.length > 0 && value.length <= 1024

/**
 * The envelope rules of the schema (MI §3): the version, a key on every
 * mutation and none on a recall, known members only, and each input's own
 * shape. A binding request never reaches a KQL/KML parser.
 */
export function parseMemoryRequest(value: unknown): MemoryRequest {
  if (!isRecord(value)) return invalid('a Memory Interface request is an object')
  for (const key of Object.keys(value)) if (!REQUEST_KEYS.has(key)) invalid(`unknown request member ${key}`)
  if (value.kip_memory !== KIP_MEMORY) invalid(`kip_memory must be "${KIP_MEMORY}"`)
  const operation = value.operation as Operation
  if (!OPERATIONS.has(operation)) invalid('unknown operation')
  if (value.request_id !== undefined && !isRef(value.request_id)) invalid('request_id is a reference')
  if (MUTATIONS.has(operation) && !isRef(value.idempotency_key)) invalid(`${operation} is a mutation and requires an idempotency_key`)
  if (!MUTATIONS.has(operation) && value.idempotency_key !== undefined) invalid('recall is read-only and takes no idempotency_key')
  if (value.requires !== undefined && (!Array.isArray(value.requires) || value.requires.some(b => typeof b !== 'string'))) invalid('requires lists level names')
  if (value.scope !== undefined) {
    if (!isRecord(value.scope)) invalid('scope is an object')
    const scope = value.scope as Record<string, unknown>
    for (const key of Object.keys(scope)) if (key !== 'task_ref' && key !== 'context_refs') invalid(`unknown scope member ${key}`)
    if (scope.task_ref !== undefined && !isRef(scope.task_ref)) invalid('task_ref is a reference')
    if (scope.context_refs !== undefined && (!Array.isArray(scope.context_refs) || !scope.context_refs.every(isRef))) invalid('context_refs are references')
  }
  if (value.budget !== undefined) {
    if (!isRecord(value.budget)) invalid('budget is an object')
    const budget = value.budget as Record<string, unknown>
    for (const key of ['max_output_tokens', 'deadline_ms']) {
      if (budget[key] !== undefined && (!Number.isSafeInteger(budget[key]) || (budget[key] as number) < 1)) invalid(`${key} is a positive integer`)
    }
  }
  if (!isRecord(value.input)) invalid('input is an object')
  const input = value.input as Record<string, unknown>
  for (const key of Object.keys(input)) if (!INPUT_KEYS[operation].has(key)) invalid(`unknown ${operation} input member ${key}`)
  switch (operation) {
    case 'observe':
      if (!isRef(input.source_ref)) invalid('observe needs a source_ref')
      break
    case 'revise':
      if (!isRef(input.source_ref)) invalid('revise needs a source_ref')
      if (input.target_ref !== undefined && !isRef(input.target_ref)) invalid('target_ref is a reference')
      if (input.change_kind !== undefined && !['correction', 'world_change', 'misrecorded', 'unspecified'].includes(input.change_kind as string)) invalid('unknown change_kind')
      break
    case 'feedback':
      for (const key of ['source_ref', 'decision_ref', 'attempt_ref']) {
        if ((key === 'source_ref' || input[key] !== undefined) && !isRef(input[key])) invalid(`${key} is a reference`)
      }
      break
    case 'forget':
      if (!isRef(input.target_ref)) invalid('forget needs a target_ref')
      if (input.mode !== 'payload_only' && input.mode !== 'semantic') invalid('forget mode is payload_only or semantic')
      break
    case 'recall': {
      const mode = input.mode ?? 'answer'
      if (!['answer', 'action', 'resume', 'attention'].includes(mode as string)) invalid('unknown recall mode')
      if (input.after !== undefined && (!Array.isArray(input.after) || input.after.length > MAX_AFTER || !input.after.every(isRef))) invalid(`after lists at most ${MAX_AFTER} receipts`)
      if (input.detail !== undefined && input.detail !== 'brief' && input.detail !== 'evidence') invalid('detail is brief or evidence')
      if (input.query === undefined && input.target_ref === undefined && mode !== 'attention' && mode !== 'resume') invalid('recall needs a query, a target_ref or mode attention')
      if (input.query !== undefined && (typeof input.query !== 'string' || !input.query.trim())) invalid('query is non-empty text')
      if (input.time !== undefined) {
        if (!isRecord(input.time)) invalid('time is an object')
        const time = input.time as Record<string, unknown>
        if (time.as_of_seq !== undefined && (!Number.isSafeInteger(time.as_of_seq) || (time.as_of_seq as number) < 0)) invalid('as_of_seq is a sequence')
        if (time.valid_at !== undefined && (typeof time.valid_at !== 'string' || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(time.valid_at))) invalid('valid_at is a KIP timestamp')
      }
      break
    }
  }
  return value as unknown as MemoryRequest
}

/**
 * The `requires` check (MI §3): omitted means `memory_basic`; an unknown or
 * unadvertised level is refused before anything runs.
 */
export function checkRequires(request: MemoryRequest): void {
  const required = request.requires?.length ? request.requires : ['memory_basic']
  for (const bundle of required) {
    if (!BUNDLES.has(bundle as Bundle) || bundle !== 'memory_basic') {
      throw new MemoryError('UnsupportedCapability', `this binding does not advertise ${bundle}`)
    }
  }
  const tokenizer = request.budget?.tokenizer
  if (tokenizer !== undefined && tokenizer !== TOKENIZER) {
    throw new MemoryError('UnsupportedCapability', `tokenizer ${JSON.stringify(tokenizer)} is not supported; this binding counts with ${TOKENIZER}`)
  }
  if ((request.budget?.deadline_ms ?? 0) > MAX_DEADLINE_MS) {
    throw new MemoryError('ResultLimitExceeded', `deadline_ms is at most ${MAX_DEADLINE_MS}`)
  }
}

/**
 * Whether a serialized result fits the output budget. The Worker carries no
 * tokenizer tables, so it bounds conservatively: every o200k token spans at
 * least one UTF-8 byte, so a result of at most N bytes is at most N tokens.
 * That never undercounts; it can refuse or trim a result that would have fit,
 * and it never treats bytes as the token count it reports.
 */
export function fitsBudget(value: unknown, maxTokens: number): boolean {
  return new TextEncoder().encode(JSON.stringify(value)).byteLength <= maxTokens
}

export function failed(request: Pick<MemoryRequest, 'operation' | 'request_id'>, error: unknown): MemoryResponse {
  return {
    kip_memory: KIP_MEMORY,
    ...(request.request_id === undefined ? {} : { request_id: request.request_id }),
    operation: request.operation,
    status: 'failed',
    error: errorOf(error),
    warnings: [],
  }
}

/**
 * A mutation's response from its progress (MI §5): `succeeded` needs
 * available progress, recorded work is `pending`, a terminal failure is
 * `failed` with its error.
 */
export function mutationResponse(
  request: MemoryRequest, receipt: Receipt, progress: Progress, result: unknown, warnings: string[],
): MemoryResponse {
  let status: Status = progress.phase === 'available' ? 'succeeded'
    : progress.phase === 'failed' ? 'failed' : progress.phase === 'processed' ? 'partial' : 'pending'
  if (status === 'succeeded' && warnings.some(w => w.startsWith('partial:'))) status = 'partial'
  if (status === 'pending' && isRecord(result) && result.status === 'partial') status = 'partial'
  return {
    kip_memory: KIP_MEMORY,
    ...(request.request_id === undefined ? {} : { request_id: request.request_id }),
    operation: request.operation,
    status,
    receipt,
    progress,
    ...(status === 'failed'
      ? { error: progress.error ?? { code: 'InternalError', message: 'processing failed' } }
      : { result }),
    warnings,
  }
}

export function coverage(scope: Scope, channels: Channels, pending: string[], unverified: string[]): Coverage {
  const complete = Object.values(channels).every(state => state !== 'incomplete') && pending.length === 0
  return { complete, scope, channels, pending_receipts: pending, unverified_preconditions: unverified, action_eligible: complete && unverified.length === 0 }
}
