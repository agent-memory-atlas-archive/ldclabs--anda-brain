import { digestParameters, runtimeOperations } from './cognitive.js'
import type {
  AiBinding,
  JsonObject,
  RecallAnswer,
  RecallPlan,
  MutationPlan,
  Usage,
} from './types.js'

/** Default model; context cost includes reference policy, role cards and ontology. */
export const DEFAULT_AI_MODEL = '@cf/meta/llama-4-scout-17b-16e-instruct'

export interface AiMessage {
  role: 'system' | 'user' | 'assistant'
  content: string
}

export interface StructuredResult<T> {
  value: T
  usage: Usage
}

export class AiResponseError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'AiResponseError'
  }
}

const MUTATION_PLAN_SCHEMA: JsonObject = {
  type: 'object',
  additionalProperties: false,
  properties: {
    types: {
      type: 'array',
      maxItems: 16,
      items: { type: 'string' },
      description:
        'UpperCamelCase Concept type names this plan needs and the Space does not ' +
        'already resolve. The host publishes them before running any command.',
    },
    predicates: {
      type: 'array',
      maxItems: 16,
      items: { type: 'string' },
      description:
        'snake_case predicate names this plan needs and the Space does not already ' +
        'resolve.',
    },
    commands: {
      type: 'array',
      maxItems: 4,
      items: { type: 'string' },
    },
    summary: { type: 'string' },
    digests: { type: 'object', description: 'Optional digest_ parameter name to canonical JSON content. Host computes SHA-256; use :digest_revision in behavior_digest. Content includes every revision attribute except behavior_digest.' },
    runtime: { type: 'array', maxItems: 4, items: {
      type: 'object', additionalProperties: false,
      properties: {
        operation: { type: 'string', enum: ['arm_watch', 'lease_task'] },
        target_ref: { type: 'string' }, expected_version: { type: 'integer', minimum: 1 },
      }, required: ['operation', 'target_ref', 'expected_version'],
    } },
  },
  required: ['types', 'predicates', 'commands', 'summary'],
}

const RECALL_PLAN_SCHEMA: JsonObject = {
  type: 'object',
  additionalProperties: false,
  properties: {
    commands: {
      type: 'array',
      maxItems: 3,
      items: { type: 'string' },
    },
  },
  required: ['commands'],
}

const RECALL_ANSWER_SCHEMA: JsonObject = {
  type: 'object',
  additionalProperties: false,
  properties: {
    answer: { type: 'string' },
    found: { type: 'boolean' },
    uncertainty: { type: 'number', minimum: 0, maximum: 1 },
  },
  required: ['answer', 'found', 'uncertainty'],
}

export async function createMutationPlan(
  ai: AiBinding,
  model: string,
  messages: AiMessage[],
): Promise<StructuredResult<MutationPlan>> {
  const result = await runStructured(ai, model, messages, MUTATION_PLAN_SCHEMA, 1800)
  return { value: validateMutationPlan(result.value), usage: result.usage }
}

export async function createRecallPlan(
  ai: AiBinding,
  model: string,
  messages: AiMessage[],
): Promise<StructuredResult<RecallPlan>> {
  const result = await runStructured(ai, model, messages, RECALL_PLAN_SCHEMA, 900)
  return { value: validateRecallPlan(result.value), usage: result.usage }
}

export async function createRecallAnswer(
  ai: AiBinding,
  model: string,
  messages: AiMessage[],
): Promise<StructuredResult<RecallAnswer>> {
  const result = await runStructured(ai, model, messages, RECALL_ANSWER_SCHEMA, 1000)
  return { value: validateRecallAnswer(result.value), usage: result.usage }
}

async function runStructured(
  ai: AiBinding,
  model: string,
  messages: AiMessage[],
  schema: JsonObject,
  maxTokens: number,
): Promise<StructuredResult<unknown>> {
  const raw = await ai.run(model, {
    messages,
    response_format: {
      type: 'json_schema',
      json_schema: schema,
    },
    max_tokens: maxTokens,
    temperature: 0.1,
  })

  const envelope = asObject(raw, 'Workers AI returned a non-object response')
  const usage = readUsage(envelope.usage)
  const response = envelope.response

  if (isObject(response)) return { value: response, usage }
  if (typeof response !== 'string') {
    throw new AiResponseError('Workers AI response did not contain structured output')
  }

  try {
    return { value: JSON.parse(stripCodeFence(response)), usage }
  } catch {
    throw new AiResponseError('Workers AI returned invalid JSON')
  }
}

function validateMutationPlan(value: unknown): MutationPlan {
  const object = asObject(value, 'invalid memory mutation plan')
  const commands = readCommands(object.commands, 4)
  if (typeof object.summary !== 'string') {
    throw new AiResponseError('memory mutation plan is missing `summary`')
  }
  return {
    commands,
    // A model that declares nothing is the common case, and it is not an error:
    // the Profile already names most of what a memory needs.
    types: readSymbols(object.types),
    predicates: readSymbols(object.predicates),
    summary: object.summary.trim(),
    parameters: digestParameters(object.digests),
    runtime: runtimeOperations(object.runtime),
  }
}

/**
 * The symbols a plan proposes.
 *
 * Shape only. Whether a name is a legal symbol, whether the Space already has
 * it, and whether it fits under the cap are the host's decisions, made where
 * the vocabulary is — not here, where a refusal would look like a malformed
 * model response.
 */
function readSymbols(value: unknown): string[] {
  if (value === undefined || value === null) return []
  if (!Array.isArray(value)) {
    throw new AiResponseError('declared symbols must be an array of strings')
  }
  return value
    .filter((item): item is string => typeof item === 'string')
    .map((item) => item.trim())
    .filter(Boolean)
    .slice(0, 16)
}

function validateRecallPlan(value: unknown): RecallPlan {
  const object = asObject(value, 'invalid recall plan')
  return { commands: readCommands(object.commands, 3) }
}

function validateRecallAnswer(value: unknown): RecallAnswer {
  const object = asObject(value, 'invalid recall answer')
  if (
    typeof object.answer !== 'string' ||
    typeof object.found !== 'boolean' ||
    typeof object.uncertainty !== 'number' ||
    !Number.isFinite(object.uncertainty)
  ) {
    throw new AiResponseError('recall answer does not match the required shape')
  }

  return {
    answer: object.answer.trim(),
    found: object.found,
    uncertainty: Math.max(0, Math.min(1, object.uncertainty)),
  }
}

function readCommands(value: unknown, max: number): string[] {
  if (!Array.isArray(value) || value.length > max) {
    throw new AiResponseError(`plan must contain at most ${max} KIP commands`)
  }
  if (!value.every((item) => typeof item === 'string')) {
    throw new AiResponseError('every KIP command must be a string')
  }
  return value.map((item) => item.trim()).filter(Boolean)
}

function readUsage(value: unknown): Usage {
  if (!isObject(value)) return { input_tokens: 0, output_tokens: 0 }
  return {
    input_tokens: readTokenCount(value.input_tokens ?? value.prompt_tokens),
    output_tokens: readTokenCount(value.output_tokens ?? value.completion_tokens),
  }
}

function readTokenCount(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value)
    ? Math.max(0, Math.trunc(value))
    : 0
}

function stripCodeFence(value: string): string {
  const trimmed = value.trim()
  const match = /^```(?:json)?\s*([\s\S]*?)\s*```$/i.exec(trimmed)
  return match?.[1] ?? trimmed
}

function asObject(value: unknown, message: string): JsonObject {
  if (!isObject(value)) throw new AiResponseError(message)
  return value
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

export function addUsage(left: Usage, right: Usage): Usage {
  return {
    input_tokens: left.input_tokens + right.input_tokens,
    output_tokens: left.output_tokens + right.output_tokens,
  }
}
