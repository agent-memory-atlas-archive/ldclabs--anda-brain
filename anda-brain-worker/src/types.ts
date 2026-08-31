import type { KipResult } from '@ldclabs/kip-do'
import type { KipExecution, KipOperation } from './kip.js'

export type JsonObject = Record<string, unknown>

/** The small subset of the Workers AI binding used by this service. */
export interface AiBinding {
  run(model: string, input: Record<string, unknown>): Promise<unknown>
}

export interface Env {
  BRAIN: DurableObjectNamespace<import('./brain.js').AndaBrain>
  AI: AiBinding
  AI_MODEL?: string
  BRAIN_API_KEY?: string
}

/**
 * The Durable Object as the Worker sees it.
 *
 * Every method is synchronous inside the object and a promise across the RPC
 * boundary, which is why these signatures do not match the class's.
 */
export interface BrainRpc {
  declareSymbols(
    types: readonly string[],
    predicates: readonly string[],
  ): Promise<DeclaredVocabulary>
  describePrimer(): Promise<KipResult>
  executeFormationPlan(operations: readonly KipOperation[]): Promise<KipResult[]>
  executeKip(command: string, params?: Record<string, unknown>): Promise<KipResult>
  executeKipBatch(
    operations: readonly KipOperation[],
    context?: undefined,
    read?: undefined,
    execution?: KipExecution,
  ): Promise<KipResult[]>
  executeKipReadonlyBatch(
    operations: readonly KipOperation[],
    execution?: KipExecution,
  ): Promise<KipResult[]>
  executeMaintenancePlan(operations: readonly KipOperation[]): Promise<KipResult[]>
  stats(): Promise<BrainStats>
  vocabulary(): Promise<DeclaredVocabulary>
}

export interface BrainStats {
  concepts: number
  propositions: number
  assertions: number
  evidence: number
  /** Every schema activation mints one; a restart is not an activation. */
  schema_environment_version: number
  initialized_at: string
  engine: string
  kip: string
}

/** What this Space can say, after a declaration and before the next one. */
export interface DeclaredVocabulary {
  package_ref: string
  types: string[]
  predicates: string[]
  /** Names refused as malformed, or past the Space's symbol cap. */
  rejected: string[]
}

export interface InputContext {
  counterparty?: string
  user?: string
  agent?: string
  source?: string
  topic?: string
}

export interface MessagePart {
  type?: string
  text?: string
  [key: string]: unknown
}

export interface Message {
  role: 'system' | 'user' | 'assistant' | 'tool'
  content: string | (string | MessagePart)[]
  name?: string
  user?: string
  timestamp?: number
}

export interface FormationInput {
  messages: Message[]
  context?: InputContext
  timestamp?: string
}

export interface RecallInput {
  query: string
  context?: InputContext
}

export interface MaintenanceInput {
  trigger?: 'scheduled' | 'threshold' | 'on_demand'
  scope?: 'full' | 'quick' | 'daydream'
  timestamp?: string
  parameters?: {
    stale_event_threshold_days?: number
    unsorted_max_backlog?: number
    orphan_max_count?: number
  }
}

export interface Usage {
  input_tokens: number
  output_tokens: number
}

/**
 * What a mutation plan asks for.
 *
 * `types` and `predicates` are the plan's schema request. KML cannot declare a
 * symbol in KIP 2.0, so a plan that needs one names it here and the host
 * publishes it before running a single command — otherwise every write using it
 * would fail with `SchemaSymbolNotFound`.
 */
export interface MutationPlan {
  commands: string[]
  types: string[]
  predicates: string[]
  summary: string
}

export interface RecallPlan {
  commands: string[]
}

export interface RecallAnswer {
  answer: string
  found: boolean
  uncertainty: number
}

/**
 * One element an answer rests on.
 *
 * `type` and `schema_ref` are present only when the read that produced this
 * projected them. KIP 2.0 shapes a KQL answer by its projection and never as
 * objects, so a citation from a model-planned read is often an id and nothing
 * else — which is honest, where a guessed type would not be.
 */
export interface MemoryCitation {
  entity: string
  type?: string
  name?: string
  schema_ref?: string
}
