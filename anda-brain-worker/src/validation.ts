import type { JsonMap } from '@ldclabs/kip-do'
import type { KipExecution, KipOperation } from './kip.js'
import type {
  FormationInput,
  InputContext,
  MaintenanceInput,
  Message,
  RecallInput,
} from './types.js'

export class ValidationError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ValidationError'
  }
}

export function parseFormationInput(value: unknown): FormationInput {
  const body = object(value, 'formation body must be a JSON object')
  if (!Array.isArray(body.messages) || body.messages.length === 0) {
    throw new ValidationError('`messages` must contain at least one message')
  }
  if (body.messages.length > 32) {
    throw new ValidationError('`messages` cannot contain more than 32 messages')
  }

  const messages = body.messages.map(parseMessage)
  const totalText = messages.reduce((sum, message) => sum + messageText(message).length, 0)
  if (totalText === 0) throw new ValidationError('messages cannot all be empty')
  if (totalText > 64_000) throw new ValidationError('message text exceeds 64000 characters')

  const context = body.context === undefined ? undefined : parseContext(body.context)
  const timestamp = optionalTimestamp(body.timestamp)
  return { messages, ...(context ? { context } : {}), ...(timestamp ? { timestamp } : {}) }
}

export function parseRecallInput(value: unknown): RecallInput {
  const body = object(value, 'recall body must be a JSON object')
  const query = requiredString(body.query, 'query', 4_000).trim()
  if (!query) throw new ValidationError('`query` cannot be blank')
  const context = body.context === undefined ? undefined : parseContext(body.context)
  return { query, ...(context ? { context } : {}) }
}

export function parseMaintenanceInput(value: unknown): MaintenanceInput {
  const body = object(value, 'maintenance body must be a JSON object')
  const trigger = body.trigger ?? 'on_demand'
  const scope = body.scope ?? 'daydream'
  if (
    typeof trigger !== 'string' ||
    !['scheduled', 'threshold', 'on_demand'].includes(trigger)
  ) {
    throw new ValidationError('invalid maintenance `trigger`')
  }
  if (typeof scope !== 'string' || !['full', 'quick', 'daydream'].includes(scope)) {
    throw new ValidationError('invalid maintenance `scope`')
  }

  const timestamp = optionalTimestamp(body.timestamp)

  let parameters: MaintenanceInput['parameters']
  if (body.parameters !== undefined) {
    const raw = object(body.parameters, '`parameters` must be an object')
    // There is deliberately no `confidence_decay_factor`. KIP 2.0 forbids
    // decaying an Assertion's confidence over time: a fact nobody has asked
    // about in a month is no less credible. What `memory_strength_decay_factor`
    // paces is `MnemonicState.memory_strength`, which is accessibility, not
    // truth. The Rust service accepts the old name as an alias; this one does
    // not accept it at all, because there is no stored history here to keep
    // compatible with.
    //
    // The names match `anda_brain`'s `MaintenanceParameters` field for field,
    // including its `unsorted_max_backlog` alias for `unconsolidated_max_backlog`:
    // two products documented as one API have to accept one request body.
    parameters = {
      memory_strength_decay_factor: boundedFraction(
        raw.memory_strength_decay_factor,
        'memory_strength_decay_factor',
      ),
      stale_event_threshold_days: boundedInteger(
        raw.stale_event_threshold_days,
        'stale_event_threshold_days',
        1,
        365,
      ),
      unconsolidated_max_backlog: boundedInteger(
        raw.unconsolidated_max_backlog ?? raw.unsorted_max_backlog,
        'unconsolidated_max_backlog',
        1,
        10_000,
      ),
      orphan_max_count: boundedInteger(
        raw.orphan_max_count,
        'orphan_max_count',
        1,
        10_000,
      ),
    }
  }

  return {
    trigger: trigger as MaintenanceInput['trigger'],
    scope: scope as MaintenanceInput['scope'],
    ...(timestamp ? { timestamp } : {}),
    ...(parameters ? { parameters } : {}),
  }
}

/** A parsed batch: what to run, and how the operations relate to one another. */
export interface KipBatch {
  operations: KipOperation[]
  execution: KipExecution
}

/**
 * The model-facing argument shape, not the wire envelope.
 *
 * A caller sends `{"command": "…"}` or an `operations` batch, exactly as the
 * Rust service does, so the two products present one API. The protocol tag and
 * the envelope's own fields are this service's business rather than every
 * client's.
 */
export function parseKipInput(value: unknown): KipBatch {
  const body = object(value, 'KIP body must be a JSON object')
  const shared = body.parameters === undefined
    ? undefined
    : jsonObject(body.parameters, '`parameters` must be an object')
  const execution = parseExecution(body.execution)

  const hasCommand = body.command !== undefined && body.command !== null
  const hasOperations = body.operations !== undefined && body.operations !== null
  if (hasCommand && hasOperations) {
    throw new ValidationError(
      'send either a single `command` or an `operations` batch, never both',
    )
  }

  if (hasCommand) {
    const command = requiredString(body.command, 'command', 256_000).trim()
    if (!command) throw new ValidationError('`command` cannot be blank')
    return {
      operations: [{ command, ...(shared ? { parameters: shared } : {}) }],
      execution,
    }
  }

  if (!Array.isArray(body.operations) || body.operations.length === 0) {
    throw new ValidationError('body must contain `command` or non-empty `operations`')
  }
  const operations = body.operations.map((item) => parseOperation(item, shared))
  const named = new Set<string>()
  for (const operation of operations) {
    if (operation.op_id === undefined) continue
    if (named.has(operation.op_id)) {
      throw new ValidationError(
        `\`op_id\` ${JSON.stringify(operation.op_id)} appears twice; it is how a ` +
          'caller pairs an answer with the operation it answers',
      )
    }
    named.add(operation.op_id)
  }
  return { operations, execution }
}

/**
 * How the batch runs (§75), defaulted rather than required.
 *
 * The wire envelope makes a multi-operation request declare this; the
 * model-facing shape does not, because the historical answer here was
 * `independent` and a caller that never asked for anything else should not
 * start getting envelope errors. What it must not do is *silently* differ from
 * what was asked, which is why `atomic` is refused and an unrecognized
 * `on_error` is refused rather than defaulted: the default it would fall into
 * is `continue`, so a sequence meant to stop would commit the writes the caller
 * asked to have skipped.
 */
function parseExecution(value: unknown): KipExecution {
  if (value === undefined || value === null) {
    return { mode: 'independent', onError: 'stop' }
  }
  const raw = object(value, '`execution` must be an object')
  const mode = raw.mode ?? 'independent'
  // Refused rather than run as a sequence that looks like one: this engine has
  // no transaction spanning several operations, and a caller that asked for
  // all-or-none must not be told it got it.
  if (mode === 'atomic') {
    throw new ValidationError(
      'this service has no atomic batch: each operation commits on its own',
    )
  }
  if (mode !== 'independent' && mode !== 'sequence') {
    throw new ValidationError(
      '`execution.mode` is one of independent, sequence or atomic',
    )
  }
  const onError = raw.on_error ?? 'stop'
  if (onError !== 'stop' && onError !== 'continue') {
    throw new ValidationError('`execution.on_error` is one of stop or continue')
  }
  return { mode, onError }
}

function parseOperation(value: unknown, shared: JsonMap | undefined): KipOperation {
  if (typeof value === 'string') {
    const command = value.trim()
    if (!command) throw new ValidationError('a KIP operation cannot be blank')
    return { command, ...(shared ? { parameters: shared } : {}) }
  }
  const raw = object(value, 'every KIP operation must be a string or an object')
  const command = requiredString(raw.command, 'operation.command', 256_000).trim()
  if (!command) throw new ValidationError('a KIP operation cannot be blank')
  // Carried through untouched and echoed on the answer. It is the caller's own
  // name for this operation, which is what makes a batch whose answers are not
  // all present — a stopped `sequence` answers `skipped` — readable without
  // counting positions.
  const opId = raw.op_id === undefined || raw.op_id === null
    ? undefined
    : requiredString(raw.op_id, 'operation.op_id', 128).trim()
  if (opId === '') throw new ValidationError('`operation.op_id` cannot be blank')
  // An operation's own bindings win over the shared ones, which is what lets a
  // batch send one `:limit` for every command and override it in exactly one.
  const own = raw.parameters === undefined
    ? undefined
    : jsonObject(raw.parameters, '`operation.parameters` must be an object')
  const parameters = own === undefined && shared === undefined
    ? undefined
    : { ...(shared ?? {}), ...(own ?? {}) }
  return {
    ...(opId === undefined ? {} : { op_id: opId }),
    command,
    ...(parameters ? { parameters } : {}),
  }
}

function parseMessage(value: unknown): Message {
  const raw = object(value, 'every message must be an object')
  if (
    typeof raw.role !== 'string' ||
    !['system', 'user', 'assistant', 'tool'].includes(raw.role)
  ) {
    throw new ValidationError('message role must be system, user, assistant, or tool')
  }
  const content = raw.content
  if (
    typeof content !== 'string' &&
    !(
      Array.isArray(content) &&
      content.every(
        (part) =>
          typeof part === 'string' ||
          (isObject(part) && (part.text === undefined || typeof part.text === 'string')),
      )
    )
  ) {
    throw new ValidationError('message content must be text or an array of text parts')
  }

  const message: Message = {
    role: raw.role as Message['role'],
    content: content as Message['content'],
  }
  if (raw.name !== undefined) message.name = requiredString(raw.name, 'message.name', 256)
  if (raw.user !== undefined) message.user = requiredString(raw.user, 'message.user', 256)
  if (raw.timestamp !== undefined) {
    if (typeof raw.timestamp !== 'number' || !Number.isFinite(raw.timestamp)) {
      throw new ValidationError('message.timestamp must be a finite Unix millisecond value')
    }
    message.timestamp = raw.timestamp
  }
  return message
}

function messageText(message: Message): string {
  if (typeof message.content === 'string') return message.content
  return message.content
    .map((part) => (typeof part === 'string' ? part : part.text ?? ''))
    .join('\n')
}

function parseContext(value: unknown): InputContext {
  const raw = object(value, '`context` must be a JSON object')
  const context: InputContext = {}
  for (const key of ['counterparty', 'user', 'agent', 'source', 'topic'] as const) {
    const field = optionalString(raw[key], `context.${key}`, 512)
    if (field) context[key] = field
  }
  if (!context.counterparty && context.user) context.counterparty = context.user
  return context
}

/**
 * A `(0, 1]` decay multiplier. Zero is excluded: a factor of zero is not slow
 * forgetting, it is erasing every memory's accessibility in one sweep.
 */
function boundedFraction(value: unknown, name: string): number | undefined {
  if (value === undefined) return undefined
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0 || value > 1) {
    throw new ValidationError(`maintenance parameter \`${name}\` must be in (0, 1]`)
  }
  return value
}

function boundedInteger(
  value: unknown,
  name: string,
  min: number,
  max: number,
): number | undefined {
  if (value === undefined) return undefined
  if (!Number.isInteger(value) || (value as number) < min || (value as number) > max) {
    throw new ValidationError(`maintenance parameter \`${name}\` must be in [${min}, ${max}]`)
  }
  return value as number
}

function requiredString(value: unknown, name: string, max: number): string {
  if (typeof value !== 'string') throw new ValidationError(`\`${name}\` must be a string`)
  if (value.length > max) throw new ValidationError(`\`${name}\` exceeds ${max} characters`)
  return value
}

function optionalString(value: unknown, name: string, max: number): string | undefined {
  if (value === undefined || value === null) return undefined
  return requiredString(value, name, max)
}

function optionalTimestamp(value: unknown): string | undefined {
  if (value === undefined || value === null) return undefined
  const timestamp = requiredString(value, 'timestamp', 64)
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})$/.exec(
    timestamp,
  )
  if (!match) throw new ValidationError('`timestamp` must be an ISO 8601 date-time')

  const [, yearText, monthText, dayText, hourText, minuteText, secondText] = match
  const year = Number(yearText)
  const month = Number(monthText)
  const day = Number(dayText)
  const hour = Number(hourText)
  const minute = Number(minuteText)
  const second = Number(secondText)
  const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate()
  if (
    month < 1 ||
    month > 12 ||
    day < 1 ||
    day > daysInMonth ||
    hour > 23 ||
    minute > 59 ||
    second > 59 ||
    Number.isNaN(Date.parse(timestamp))
  ) {
    throw new ValidationError('`timestamp` must be an ISO 8601 date-time')
  }
  return timestamp
}

/**
 * The same check as {@link object}, for a value that reaches a KIP parameter.
 *
 * The cast is safe by construction rather than by inspection: the whole body
 * came out of `JSON.parse`, so every value in it is already JSON — and walking
 * it again to prove that to the compiler would reject nothing.
 */
function jsonObject(value: unknown, message: string): JsonMap {
  return object(value, message) as JsonMap
}

function object(value: unknown, message: string): Record<string, unknown> {
  if (!isObject(value)) throw new ValidationError(message)
  return value
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
