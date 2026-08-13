import type { KipResponse } from '@ldclabs/kip-do'

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
  TOKENIZER?: {
    fetch(input: RequestInfo, init?: RequestInit): Promise<Response>
  }
}

export interface BrainRpc {
  describePrimer(): Promise<KipResponse>
  executeFormationPlan(commands: string[]): Promise<KipResponse[]>
  executeKip(command: string): Promise<KipResponse>
  executeKipBatch(commands: string[]): Promise<KipResponse[]>
  executeKipReadonlyBatch(commands: string[]): Promise<KipResponse[]>
  executeMaintenancePlan(commands: string[]): Promise<KipResponse[]>
  stats(): Promise<BrainStats>
}

export interface BrainStats {
  concepts: number
  propositions: number
  initialized_at: string
  engine: string
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
    confidence_decay_factor?: number
    unsorted_max_backlog?: number
    orphan_max_count?: number
  }
}

export interface Usage {
  input_tokens: number
  output_tokens: number
}

export interface MutationPlan {
  commands: string[]
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

export interface MemoryCitation {
  entity: string
  type?: string
  name?: string
  confidence?: number
  source?: string
  created_at?: string
}
