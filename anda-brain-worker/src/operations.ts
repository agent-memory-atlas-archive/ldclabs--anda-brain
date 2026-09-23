import { assertCurrentOperations, type SourceIdentity } from './product.js'
import { contentDigest, type KipResult } from '@ldclabs/kip-do'
import { observationTimestamp } from './validation.js'
import {
  DEFAULT_AI_MODEL,
  AiResponseError,
  ModelDeadline,
  addUsage,
  createMutationPlan,
  createRecallAnswer,
  createRecallPlan,
} from './ai.js'
import {
  MAX_KIP_OPERATIONS,
  assertFormationOperations,
  assertMaintenanceOperations,
  citationsFrom,
  conceptLookupCommand,
  countChanges,
  countWrites,
  keepReadonlyOperations,
  observationIngest,
  type KipOperation,
} from './kip.js'
import {
  formationMessages,
  formationReviewMessages,
  maintenanceMessages,
  recallAnswerMessages,
  recallPlanMessages,
} from './prompts.js'
import type {
  BrainRpc,
  DeclaredVocabulary,
  Env,
  FormationInput,
  MaintenanceInput,
  MutationPlan,
  RecallInput,
  Usage,
} from './types.js'

const EMPTY_USAGE: Usage = { input_tokens: 0, output_tokens: 0 }

export class OperationError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly data?: unknown,
  ) {
    super(message)
    this.name = 'OperationError'
  }
}

export async function formMemory(
  env: Env,
  brain: BrainRpc,
  input: FormationInput,
  sourceIdentity?: SourceIdentity,
): Promise<unknown> {
  const timestamp = observationTimestamp(input.timestamp, Date.now())
  const origin = await conversationOrigin(input, timestamp)
  const epoch = await brain.beginProcessing(sourceIdentity ?? { key: origin,
    parents: input.context?.source ? [`source:${contentDigest(input.context.source)}`] : [] }, origin)
  const deadline = new ModelDeadline(env.AI_TIMEOUT_MS)
  let usage = { ...EMPTY_USAGE }
  try {
    const counterparty = await counterpartyElement(brain, input.context?.counterparty, epoch)
    const primer = resultOrThrow(await brain.describePrimer(), 'formation primer failed')
    const model = env.AI_MODEL || DEFAULT_AI_MODEL
    const plan = await createMutationPlan(
      env.AI,
      model,
      formationMessages(primer, input, timestamp),
      deadline,
    )

    usage = plan.usage
    deadline.check()
    if (plan.value.reviewed_corrections?.length) throw new OperationError('Formation cannot acknowledge maintenance corrections', 422)
    if (plan.value.runtime?.length) throw new OperationError('Formation cannot manage Watch arming or task leases', 422)

    const { operations, vocabulary } = await preparePlan(
      brain,
      plan.value,
      assertFormationOperations,
      'formation',
      epoch,
    )

    // The observation rides the envelope, so the model's commands cite `:msg1`
    // instead of retyping what was said (§71.1).
    const ingest = observationIngest(operations, input.messages, {
      at: timestamp,
      origin,
      sourceActor: counterparty.id,
    })
    deadline.check()
    const results = operations.length
      ? await brain.executeFormationPlan(operations, ingest, epoch)
      : []
    throwOnKipError(results, 'formation KIP failed')
    let review: { performed: boolean; commands: number; summary: string } | undefined
    let repairResults: KipResult[] = []
    // Same 10K-token cost heuristic as Rust (approximately four characters per
    // token). Count the original input, never the compacted prompt or references.
    if (JSON.stringify(input).length >= 40_000) {
      try {
        const repair = await createMutationPlan(env.AI, model,
          formationReviewMessages(primer, input, timestamp, results), deadline)
        usage = addUsage(usage, repair.usage)
        deadline.check()
        if (repair.value.reviewed_corrections?.length) throw new OperationError('Formation review cannot acknowledge maintenance corrections', 422)
        if (repair.value.commands.length > 1) throw new OperationError('Formation review accepts at most one repair MUTATE', 422)
        if (repair.value.runtime?.length) throw new OperationError('Formation review cannot manage runtime actions', 422)
        const prepared = await preparePlan(brain, repair.value, assertFormationOperations, 'formation review', epoch)
        const repairIngest = observationIngest(prepared.operations, input.messages, {at: timestamp, origin, sourceActor: counterparty.id})
        deadline.check()
        repairResults = prepared.operations.length ? await brain.executeFormationPlan(prepared.operations, repairIngest, epoch) : []
        throwOnKipError(repairResults, 'formation review KIP failed')
        review = { performed:true, commands:prepared.operations.length, summary:repair.value.summary }
      } catch (error) {
        if (error instanceof AiResponseError) usage = addUsage(usage, error.usage)
        throw new OperationError('formation review failed after initial writes; inspect receipts before retrying', 422,
          {initial_results:results, repair_results:repairResults, usage})
      }
    }
    const stored = countWrites([...results, ...repairResults])
    stored.concepts += counterparty.created

    return {
      content: committedContent(plan.value.summary, [...results, ...repairResults], 'No memory-plan changes were committed.'),
      operation_results: operationStatuses([...results, ...repairResults]),
      stored,
      commands: operations.length + (review?.commands ?? 0),
      ...(vocabulary ? { vocabulary } : {}),
      ...(review ? { review } : {}),
      usage,
    }
  } catch (error) { throw withUsage(error, usage) }
  finally { deadline.close(); await brain.checkProcessing(epoch) }
}

/**
 * The stable name this conversation's minted Evidence is keyed under.
 *
 * The digest covers the complete input, including source and counterparty.
 * A source names a thread/channel, not an individual observation. Reusing a
 * thread must never replace a new message's Evidence with that thread's first
 * message. Callers retrying an observation keep its explicit timestamp; without
 * one, each request receives a new observation time.
 */
async function conversationOrigin(
  input: FormationInput,
  timestamp: string,
): Promise<string> {
  const bytes = new TextEncoder().encode(
    JSON.stringify({ messages: input.messages, context: input.context ?? {}, timestamp }),
  )
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))
  const hex = Array.from(digest, (byte) =>
    byte.toString(16).padStart(2, '0'),
  ).join('')
  return `formation:sha256:${hex}`
}

/**
 * Resolve/create the semantic counterparty before planning. First ingestion
 * and a retry must name the same Evidence source, even when the first model
 * plan would otherwise be responsible for creating the Person.
 */
async function counterpartyElement(
  brain: BrainRpc,
  counterparty: string | undefined,
  epoch: number,
): Promise<{ id?: string; created: number }> {
  if (!counterparty) return { created: 0 }
  const lookup = {
    command: 'FIND(?person.id) WHERE { ?person CONCEPT {type: "Person", key: :key} } LIMIT 1',
    parameters: { key: counterparty },
  }
  const [known] = await brain.executeAgentRead([lookup], epoch)
  if (known?.status === 'failed') throw new OperationError('counterparty lookup failed', 422, known.error)
  const knownId = Array.isArray(known?.result) ? known.result[0] : undefined
  if (typeof knownId === 'string') return { id: knownId, created: 0 }
  const created = await brain.executeFormationPlan([{
    command: 'CREATE CONCEPT ?person { TYPE "Person" NAME :key CLIENT KEY :creation_key SET FIELDS {key: :key} }',
    parameters: { key: counterparty, creation_key: `anda-brain:counterparty:${counterparty}` },
  }], undefined, epoch)
  // A concurrent creation may win the key. Reading the winner preserves its
  // display name; an UPSERT here could overwrite a rename made meanwhile.
  const collision = created.some((result) => result.error?.code === 'IdentityConflict')
  if (!collision) throwOnKipError(created, 'counterparty initialization failed')
  const [found] = await brain.executeAgentRead([lookup], epoch)
  if (found?.status !== 'succeeded') throw new OperationError('counterparty lookup failed', 422, found?.error)
  const row = Array.isArray(found.result) ? found.result[0] : undefined
  const count = countWrites(created).concepts
  if (typeof row === 'string') return { id: row, created: count }
  const id = (row as { id?: unknown } | undefined)?.id
  if (typeof id === 'string') return { id, created: count }
  throw new OperationError('counterparty lookup returned no element', 422)
}

export async function recallMemory(
  env: Env,
  brain: BrainRpc,
  input: RecallInput,
): Promise<unknown> {
  const epoch = await brain.beginProcessing()
  const deadline = new ModelDeadline(env.AI_TIMEOUT_MS)
  let usage = { ...EMPTY_USAGE }
  try {
    const model = env.AI_MODEL || DEFAULT_AI_MODEL
    const primer = resultOrThrow(await brain.describePrimer(), 'recall primer failed')
    const lookup = conceptLookupCommand(input.query, 8)
    const [grounding] = await brain.executeAgentRead([lookup], epoch)
    if (grounding === undefined || grounding.status !== 'succeeded') {
      throw new OperationError('recall KIP failed', 422, grounding?.error)
    }
    let planned: KipOperation[] = []
    let plannerWarning: string | undefined

    try {
      const plan = await createRecallPlan(env.AI, model, recallPlanMessages(primer, input, grounding), deadline)
      usage = addUsage(usage, plan.usage)
      planned = keepReadonlyOperations(plan.value.commands.map((command) => ({ command }))).filter(operation => {
        try { if (epoch > 0) assertCurrentOperations([operation]); return true } catch { return false }
      })
    } catch (error) {
      if (error instanceof AiResponseError && error.code === 'model_timeout') throw error
      if (error instanceof AiResponseError) usage = addUsage(usage, error.usage)
      plannerWarning = error instanceof Error ? error.message : String(error)
    }

    const operations = [lookup, ...planned].slice(0, MAX_KIP_OPERATIONS)
    const results = [grounding, ...(planned.length ? await brain.executeAgentRead(operations.slice(1), epoch) : [])]

    const planFailures = results
      .slice(1)
      .flatMap((result) => (result.status === 'failed' ? [result.error?.code ?? 'Unknown'] : []))
    const memories = citationsFrom(grounding, results.slice(1))
    const answer = await createRecallAnswer(
      env.AI,
      model,
      recallAnswerMessages(input, results),
      deadline,
    )
    usage = addUsage(usage, answer.usage)

    return {
      content: answer.value.answer,
      answer: answer.value.answer,
      // The model's own report, not `&& memories.length > 0`. A citation is an
      // element id lifted out of whatever the reads happened to project, and a
      // `BELIEF` projection or an aggregate answers with values rather than ids
      // — so ANDing the two reported "answered from absence" for answers that
      // had evidence. That is the `insufficient` / `rejected` collapse the
      // policy exists to prevent, arriving through the transport instead of the
      // prose. Where the model reports nothing found, `memories` is the
      // fallback.
      found: answer.value.found,
      uncertainty: answer.value.uncertainty,
      memories,
      usage,
      diagnostics: {
        kip_commands: operations.length,
        ...(plannerWarning ? { planner_warning: plannerWarning } : {}),
        ...(planFailures.length ? { planned_read_errors: planFailures } : {}),
      },
    }
  } catch (error) { throw withUsage(error, usage) }
  finally { deadline.close(); await brain.checkProcessing(epoch) }
}

/**
 * A memory lookup with no model in it.
 *
 * The same deterministic `SEARCH` that grounds recall, answered on its own. A
 * miss is a miss on the index, not an absence, which is why the answer reports
 * what it found rather than whether the memory exists.
 */
export async function probeMemory(
  brain: BrainRpc,
  input: RecallInput,
): Promise<unknown> {
  const epoch = await brain.beginProcessing()
  const [probe] = await brain.executeAgentRead([conceptLookupCommand(input.query, 8)], epoch)
  await brain.checkProcessing(epoch)
  if (probe === undefined || probe.status === 'failed') {
    throw new OperationError('probe KIP failed', 422, probe?.error)
  }
  const memories = citationsFrom(probe)
  return { found: memories.length > 0, memories }
}

export async function maintainMemory(
  env: Env, brain: BrainRpc, input: MaintenanceInput,
): Promise<unknown> {
  const epoch = await brain.beginProcessing()
  const deadline = new ModelDeadline(env.AI_TIMEOUT_MS)
  let run: string | undefined
  let usage = { ...EMPTY_USAGE }
  try {
    run = await brain.beginMaintenance(epoch, deadline.expiresAt)
    const timestamp = observationTimestamp(input.timestamp, Date.now())
    const settlement = await brain.settleMemory(Date.parse(timestamp), input.parameters?.memory_strength_decay_factor, run, epoch)
    const [snapshot, assessment, primerResult] = await Promise.all([
      brain.maintenanceSnapshot(epoch, run), brain.maintenanceAssessment(), brain.describePrimer(),
    ])
    throwOnKipError(snapshot, 'maintenance snapshot failed')
    const primer = resultOrThrow(primerResult, 'maintenance primer failed')
    const plan = await createMutationPlan(env.AI, env.AI_MODEL || DEFAULT_AI_MODEL,
      maintenanceMessages(input, { snapshot, assessment, settlement }, timestamp, primer), deadline)
    usage = plan.usage
    deadline.check()
    if (plan.value.reviewed_corrections?.some(id => !assessment.revised_roots.some(root => root.assertion === id))) {
      throw new OperationError('reviewed_corrections must name roots in this maintenance snapshot', 422)
    }
    const { operations, vocabulary } = await preparePlan(brain, plan.value,
      assertMaintenanceOperations, 'maintenance', epoch, run)
    deadline.check()
    const results = await brain.executeMaintenancePlan(operations, plan.value.runtime, epoch, run, plan.value.reviewed_corrections)
    throwOnKipError(results, 'maintenance KIP failed')
    return {
      content: committedContent(plan.value.summary, results, 'No maintenance-plan changes were committed.'),
      scope: input.scope ?? 'daydream', changed: countChanges(results), commands: operations.length,
      operation_results: operationStatuses(results), settlement,
      runtime: results.filter(result => result.op_id?.startsWith('runtime_')),
      ...(vocabulary ? { vocabulary } : {}), usage,
    }
  } catch (error) { throw withUsage(error, usage) }
  finally {
    deadline.close()
    if (run) await brain.endMaintenance(run)
    await brain.checkProcessing(epoch)
  }
}

/**
 * Turns a mutation plan into runnable operations: gate first, then schema.
 *
 * The order is the whole point, and both writing modes need it. Parsing
 * resolves no symbols, so the gate does not need the vocabulary — and declaring
 * first would let a plan the gate is about to refuse still mint a schema
 * version and spend part of the Space's symbol cap on words nothing ever wrote.
 * Schema then goes in before any command runs, because KML cannot declare a
 * symbol: a command naming one the Space does not resolve fails with
 * `SchemaSymbolNotFound` and takes the whole statement with it.
 *
 * A refused *symbol* is not fatal, unlike a refused command: the plan's other
 * commands are still writable, and the one that needed it will fail on its own
 * with an error naming the symbol — a better message than a blanket rejection.
 */
async function preparePlan(
  brain: BrainRpc,
  plan: MutationPlan,
  gate: (operations: readonly KipOperation[]) => void,
  mode: string,
  epoch: number,
  run?: string,
): Promise<{ operations: KipOperation[]; vocabulary?: DeclaredVocabulary }> {
  const operations = plan.commands.map((command) => ({ command, parameters: plan.parameters }))
  try {
    if (operations.length > 0) gate(operations)
    if (epoch > 0) assertCurrentOperations(operations)
  } catch (error) {
    throw new OperationError(
      error instanceof Error ? error.message : `invalid ${mode} plan`,
      422,
    )
  }
  if (plan.types.length === 0 && plan.predicates.length === 0) return { operations }
  return {
    operations,
    vocabulary: await brain.declareSymbols(plan.types, plan.predicates, epoch, run),
  }
}

function resultOrThrow(result: KipResult, message: string): unknown {
  if (result.status === 'failed') throw new OperationError(message, 422, result.error)
  return result.result
}

function throwOnKipError(results: readonly KipResult[], message: string): void {
  const failure = results.find((result) => result.status === 'failed')?.error
  if (failure) throw new OperationError(message, 422, { error: failure, results })
}

function operationStatuses(results: readonly KipResult[]) {
  return results.map(({op_id, status, receipt}) => ({
    ...(op_id === undefined ? {} : {op_id}), status, ...(receipt ? {receipt} : {}),
  }))
}

function committedContent(summary: string, results: readonly KipResult[], empty: string): string {
  return countChanges(results).total > 0 ? summary || 'Memory changes committed.' : empty
}

function withUsage(error: unknown, usage: Usage): unknown {
  if (error instanceof AiResponseError) error.usage = addUsage(usage, error.usage)
  if (error instanceof OperationError) {
    const data = error.data && typeof error.data === 'object' ? error.data : { detail: error.data }
    return new OperationError(error.message, error.status, { usage, ...data })
  }
  return error
}
