import type { SourceIdentity } from './product.js'
import type { RuntimeOperation } from './cognitive.js'
import type { KipResult } from '@ldclabs/kip-do'
import type { IngestContext, KipExecution, KipOperation } from './kip.js'
import type { AttentionRecall, AttentionRecallInput } from './attention.js'

export type JsonObject = Record<string, unknown>

/** The small subset of the Workers AI binding used by this service. */
export interface AiBinding {
  run(model: string, input: Record<string, unknown>, options?: { signal?: AbortSignal }): Promise<unknown>
}

export interface Env {
  BRAIN: DurableObjectNamespace<import('./brain.js').AndaBrain>
  AI: AiBinding
  AI_MODEL?: string
  /** Shared wall-clock budget across all model stages, default 120000; max 300000. */
  AI_TIMEOUT_MS?: string
  BRAIN_API_KEY?: string
  /** Explicit native principal allowed to own record watches; never inferred from API key. */
  BRAIN_PRODUCT_RECIPIENT?: string
}

/**
 * The Durable Object as the Worker sees it.
 *
 * Every method is synchronous inside the object and a promise across the RPC
 * boundary, which is why these signatures do not match the class's.
 */
export interface BrainRpc {
  forgetMemory(input: import('./forget.js').ForgetInput): Promise<import('./forget.js').ForgetReport>
  beginProcessing(source?: SourceIdentity, origin?: string): Promise<number>
  checkProcessing(epoch: number): Promise<void>
  executeAgentRead(operations: readonly KipOperation[], epoch: number): Promise<KipResult[]>
  declareSymbols(
    types: readonly string[],
    predicates: readonly string[],
    epoch?: number,
  ): Promise<DeclaredVocabulary>
  describePrimer(): Promise<KipResult>
  executeFormationPlan(
    operations: readonly KipOperation[],
    ingest?: IngestContext,
    epoch?: number,
  ): Promise<KipResult[]>
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
  executeMaintenancePlan(operations: readonly KipOperation[], runtime?: readonly RuntimeOperation[], epoch?: number, run?: string, reviewed?: string[]): Promise<KipResult[]>
  beginMaintenance(epoch: number, expiresAt: number): Promise<string>
  endMaintenance(id: string): Promise<void>
  maintenanceSnapshot(epoch: number, run: string): Promise<KipResult[]>
  acknowledgeCorrections(ids: string[], epoch: number): Promise<void>
  maintenanceAssessment(): Promise<MaintenanceAssessment>
  settleMemory(nowMs: number, run?: string, epoch?: number): Promise<SettlementReport>
  stats(): Promise<BrainStats>
  vocabulary(): Promise<DeclaredVocabulary>
  schemaDrafts(): Promise<SchemaDrafts>
  promoteDraftSymbol(input: PromoteDraftInput): Promise<PromoteDraftOutput>
  recallAttention(input: AttentionRecallInput): Promise<AttentionRecall>
  memoryStage(namespace: string, space: string, input: import('./memory-ledger.js').StageSourceInput): Promise<import('./memory-ledger.js').StagedSourceRef>
  memorySource(namespace: string, sourceRef: string): Promise<import('./memory-ledger.js').StagedSource>
  memoryAdmit(namespace: string, space: string, request: import('./memory-wire.js').MemoryRequest): Promise<import('./memory-ledger.js').Admission>
  memoryCaptureEvidence(receiptRef: string, messages: Message[], observedAt: string, purpose: 'feedback' | 'revise-report', about?: Record<string, unknown>): Promise<import('./memory-ledger.js').IntakeRecord>
  memoryFinishFormation(receiptRef: string, trace: import('./memory-ledger.js').PassTrace): Promise<import('./memory-ledger.js').IntakeRecord>
  memoryFailFormation(receiptRef: string, error: { code: string; message: string }): Promise<import('./memory-ledger.js').IntakeRecord>
  memoryForget(namespace: string, space: string, request: import('./memory-wire.js').MemoryRequest, owner: boolean): Promise<import('./memory-ledger.js').IntakeRecord>
  memoryBarrier(namespace: string, after: string[]): Promise<import('./memory-wire.js').Progress[]>
  memoryScopedAttention(scope: import('./memory-wire.js').Scope | undefined, items: AttentionRecall['items']): Promise<import('./memory-wire.js').AttentionItem[]>
  memoryDeliver(namespace: string, scope: import('./memory-wire.js').Scope | undefined, cited: string[], options: Parameters<import('./memory-ledger.js').MemoryLedger['deliver']>[3]): Promise<import('./memory-wire.js').Briefing>
  memoryExpand(namespace: string, target: string, evidence: boolean): Promise<import('./memory-wire.js').Briefing>
  memoryReceipt(namespace: string, receiptRef: string): Promise<Record<string, unknown>>
  memoryPlan(namespace: string, planRef: string): Promise<Record<string, unknown>>
}

/** This Space's draft vocabulary (Spec §20.16), as its owner reviews it. */
export interface SchemaDrafts {
  /** Always `kip://local/draft@0.0.0`. */
  package_ref: string
  schema_environment_version: number
  symbols: DraftSymbol[]
}

export interface DraftSymbol {
  kind: 'ConceptType' | 'PredicateType'
  name: string
  /** The exact reference elements written under it keep forever. */
  ref: string
  definition: unknown
  /** The lineage it was promoted to, once promoted. */
  promoted_to?: string
}

/** A draft symbol promotion (Spec §20.16), the owner's Schema migration. */
export interface PromoteDraftInput {
  kind: 'ConceptType' | 'PredicateType'
  /** The draft symbol's local name or exact `kip://local/draft@0.0.0/…` ref. */
  from: string
  /** The installed symbol of the same kind: an exact ref, or an unambiguous local name. */
  to: string
}

export interface PromoteDraftOutput {
  promoted: string
  to: string
  schema_environment_version: number
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

/** What this Space's own vocabulary holds, and what one drafting call did. */
export interface DeclaredVocabulary {
  /** The legacy host package in force, or null for a Space that never had one. */
  package_ref: string | null
  /** Symbols of the legacy host package; nothing is added to it any more. */
  types: string[]
  predicates: string[]
  /** Always `kip://local/draft@0.0.0` (Spec §20.16). */
  draft_package: string
  draft_types: string[]
  draft_predicates: string[]
  /** Exact references this call drafted. */
  defined: string[]
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
    unconsolidated_max_backlog?: number
    orphan_max_count?: number
  }
}

/** One armed or fired Watch, as the maintenance cycle receives it. */
export interface ArmedWatch {
  id: string
  /** Exact persisted type; 2.0 records require explicit replacement. */
  schema_ref?: string
  /** Whole-element version used by protected runtime operations. */
  version?: number
  name: string
  /** `delta` (fire on a matching change) or `silence` (fire when `due_at` passes). */
  watch_class: string
  /** What counts as a matching change; the Profile fixes no condition language. */
  condition: string
  summary: string
  due_at: string
}

export interface WatchSettlement {
  /** Delta or silence Watches fired by native advancement. */
  fired: number
  /**
   * Watches that were due but whose fire did not commit — the maintenance
   * model changed the same Watch between the scan and the write, which
   * `EXPECT VERSION` refuses rather than clobbers. They stay armed.
   */
  conflicted: number
  /**
   * Silence Watches whose awaited change arrived before their deadline —
   * evaluated by the runtime against the Change Stream and stood down,
   * because the silence they were armed for can no longer happen.
   */
  disarmed: number
  /**
   * Silence Watches past their deadline that were held rather than fired: the
   * Change Stream had not yet been consumed through the coordinate current at
   * the deadline (Profile §5.11), so "nothing matched" was not yet a fact.
   * They fire once it has.
   */
  deferred: number
  error?: string
}

/** What correction discovery found, and where the next scan starts. */
export interface CorrectionScan {
  /** The revisions found, each with the cognition derived from it. */
  revised_roots: RevisedRoot[]
  /** The coordinate the next scan reads after. */
  cursor: number
  /** Resume within cursor's transaction; absent when it was completely read. */
  cursor_after_id?: string
  /** The bounded scan did not prove the backlog exhausted. */
  incomplete: boolean
  error?: string
}

export interface CorrectionCursor {
  seq: number
  after_id: string
}

/** One Assertion an actor superseded, with what was derived from it. */
export interface RevisedRoot {
  /** The superseded Assertion. */
  assertion: string
  /** The Proposition it took a stance on. */
  proposition?: string
  /** The actor whose claim was revised. */
  actor?: string
  /** The Assertions that superseded it. */
  superseded_by: string[]
  /** The coordinate the supersession committed at. */
  space_seq: number
  /** What `LIST DEPENDENTS` reached from the Assertion, nearest first. */
  dependents: Dependent[]
  /**
   * Set when the list is known to be incomplete: the traversal was cut by an
   * element this Principal may not discover (§63.5), the page was full, or
   * the read failed.
   */
  truncated: boolean
}

/** One element reached by a derivation walk (§63.5). */
export interface Dependent {
  id: string
  kind: string
  distance: number
  /** The Activity through which it was reached. */
  via?: string
}

export interface SkillSettlement {
  /** No evaluation ran without a configured observer/trial/evaluation pipeline. */
  unsupported_reason?: string
  /** Skills whose tallies or standing moved. */
  graded: number
  /** Lifecycle transitions recorded as `lifecycle_verdict` Activities. */
  transitions: number
  /** Verdicts refused by `EXPECT VERSION`; the cursor did not advance. */
  conflicted: number
  error?: string
}

/** What the deterministic settlement did before the cycle's completion. */
/**
 * What the Commitment review raised this cycle (Profile §5.7, §17): one
 * `commitment_review` Activity per due `pending` / `blocked` Commitment without
 * a Watch, keyed `commitment_review:<id>:<due_at>`, so a replay raises nothing
 * and only a new `due_at` raises a Commitment again.
 */
export interface CommitmentSettlement {
  due: number
  raised: number
  error?: string
}

export interface SettlementReport {
  settled_at: string
  watches: WatchSettlement
  commitments: CommitmentSettlement
  skills: SkillSettlement
  corrections: CorrectionScan
  error?: string
}

/**
 * What the settlement measured, handed to the maintenance prompt.
 *
 * Runtime-filled and not something a caller can set: a request body deciding
 * what the Brain believes about its own graph would be cognitive content
 * choosing its own evidence.
 */
export interface MaintenanceAssessment {
  /**
   * The Space's sequence coordinate. The `basis_seq` a refreshed
   * `WorkingState` is stamped with, and the coordinate `CHANGES AFTER SEQ`
   * reads from.
   */
  space_seq: number
  /** Legacy field, no longer populated; Nexus owns per-Watch consumed_seq. */
  consumed_seq?: number
  /** What the Brain is still waiting for — the delta evaluation's input. */
  armed_watches: ArmedWatch[]
  /** Fired and undecided — the action gate's queue. */
  fired_watches: ArmedWatch[]
  /** Registered predicate to link count; vocabulary sprawl is visible here. */
  predicates: Record<string, number>
  /**
   * Assertions an actor superseded since the last cycle, each with the
   * cognition derived from it — the derivation review's input (§57.5). The
   * runtime walks `LIST DEPENDENTS` so the cycle does not have to guess which
   * artifacts a revised root fed; a listed dependent is a candidate for a
   * `review_derived` SleepTask, not already stale.
   */
  revised_roots: RevisedRoot[]
}

export interface Usage {
  input_tokens: number | null
  output_tokens: number | null
  /** Known subtotals when at least one call did not report complete usage. */
  known?: { input_tokens: number; output_tokens: number }
}

/**
 * What a mutation plan asks for.
 *
 * `types` and `predicates` are bare names Formation wants drafted without
 * writing a `DEFINE` of its own: the host drafts each with a generic
 * description before running a single command (Spec §20.16). A `DEFINE`
 * among the commands does the same with the model's own description.
 * Maintenance never drafts.
 */
export interface MutationPlan {
  /** Host-computed digest bindings; never model-supplied authentication. */
  parameters?: import('@ldclabs/kip-do').JsonMap
  runtime?: RuntimeOperation[]
  reviewed_corrections?: string[]
  commands: string[]
  types: string[]
  predicates: string[]
  summary: string
}

/**
 * The model's read-only query plan for one Recall. Not KIP's `RecallPlan`, the
 * pinned selector/method/scope plan of the Memory Interface, which this Worker
 * does not implement.
 */
export interface RecallReadPlan {
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
