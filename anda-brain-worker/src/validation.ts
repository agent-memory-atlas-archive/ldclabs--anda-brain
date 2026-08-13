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
    parameters = {
      stale_event_threshold_days: boundedInteger(
        raw.stale_event_threshold_days,
        'stale_event_threshold_days',
        1,
        365,
      ),
      confidence_decay_factor: boundedNumber(
        raw.confidence_decay_factor,
        'confidence_decay_factor',
        Number.MIN_VALUE,
        1,
      ),
      unsorted_max_backlog: boundedInteger(
        raw.unsorted_max_backlog,
        'unsorted_max_backlog',
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

export function parseKipInput(value: unknown): {
  command?: string
  commands?: string[]
} {
  const body = object(value, 'KIP body must be a JSON object')
  if (typeof body.command === 'string' && body.command.trim()) {
    return { command: body.command }
  }
  if (
    Array.isArray(body.commands) &&
    body.commands.length > 0 &&
    body.commands.every((item) => typeof item === 'string' && item.trim())
  ) {
    return { commands: body.commands as string[] }
  }
  throw new ValidationError('body must contain `command` or non-empty `commands`')
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

function boundedNumber(
  value: unknown,
  name: string,
  minExclusive: number,
  max: number,
): number | undefined {
  if (value === undefined) return undefined
  if (
    typeof value !== 'number' ||
    !Number.isFinite(value) ||
    value < minExclusive ||
    value > max
  ) {
    throw new ValidationError(`maintenance parameter \`${name}\` must be in (0, ${max}]`)
  }
  return value
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

function object(value: unknown, message: string): Record<string, unknown> {
  if (!isObject(value)) throw new ValidationError(message)
  return value
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
