import type { KipResult } from '@ldclabs/kip-do'
import {
  DEFAULT_AI_MODEL,
  addUsage,
  createMutationPlan,
  createRecallAnswer,
  createRecallPlan,
} from './ai.js'
import {
  MAX_KIP_OPERATIONS,
  assertFormationOperations,
  assertMaintenanceOperations,
  citationsFromLookup,
  collectCitations,
  conceptLookupCommand,
  countChanges,
  countWrites,
  firstKipError,
  keepReadonlyOperations,
  type KipOperation,
} from './kip.js'
import {
  formationMessages,
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
  RecallInput,
  Usage,
} from './types.js'

const EMPTY_USAGE: Usage = { input_tokens: 0, output_tokens: 0 }

/**
 * What Maintenance is shown before it plans.
 *
 * Three bounded reads rather than a free look: the model gets one completion,
 * so what it does not see here it cannot go and fetch. Recent Events are the
 * consolidation backlog, open SleepTasks are the work it left itself, and the
 * longest-untouched Concepts are where consolidation and re-encoding have
 * something to say.
 *
 * That third read orders by `updated_at`, not by `MnemonicState.memory_strength`:
 * a Concept the model never gave the Facet has no strength to sort on, and
 * sorting on a mostly-absent field would rank the graph by which memories
 * happened to be annotated. `?c.facets` still rides along, so a Concept that
 * does carry one can be judged on it.
 */
const MAINTENANCE_SNAPSHOT: KipOperation[] = [
  {
    command:
      'FIND(?e.id, ?e.name, ?e.updated_at) WHERE { ?e CONCEPT {type: "Event"} } ' +
      'ORDER BY ?e.updated_at DESC LIMIT 20',
  },
  {
    command:
      'FIND(?t.id, ?t.name, ?t.attributes) WHERE { ?t CONCEPT {type: "SleepTask"} } LIMIT 20',
  },
  {
    command:
      'FIND(?c.id, ?c.name, ?c.schema_ref, ?c.facets) WHERE { ?c CONCEPT {} } ' +
      'ORDER BY ?c.updated_at ASC LIMIT 20',
  },
]

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
): Promise<unknown> {
  const timestamp = input.timestamp ?? new Date().toISOString()
  const primer = resultOrThrow(await brain.describePrimer(), 'formation primer failed')
  const model = env.AI_MODEL || DEFAULT_AI_MODEL
  const plan = await createMutationPlan(
    env.AI,
    model,
    formationMessages(primer, input, timestamp),
  )

  // Gate before publishing. Parsing resolves no symbols, so the gate does not
  // need the vocabulary — and declaring first would let a plan the gate is
  // about to refuse still mint a schema version and spend part of the Space's
  // symbol cap on words nothing ever wrote.
  const operations = plan.value.commands.map((command) => ({ command }))
  try {
    if (operations.length > 0) assertFormationOperations(operations)
  } catch (error) {
    throw new OperationError(
      error instanceof Error ? error.message : 'invalid formation plan',
      422,
    )
  }

  // Then schema, before any command runs: KML cannot declare a symbol, so a
  // command naming one the Space does not resolve fails with
  // `SchemaSymbolNotFound` and takes the whole statement with it.
  const vocabulary = await declareVocabulary(brain, plan.value)

  const results = operations.length ? await brain.executeFormationPlan(operations) : []
  throwOnKipError(results, 'formation KIP failed')

  return {
    content: plan.value.summary || 'No durable memory was extracted.',
    stored: countWrites(results),
    commands: operations.length,
    ...(vocabulary ? { vocabulary } : {}),
    usage: plan.usage,
  }
}

export async function recallMemory(
  env: Env,
  brain: BrainRpc,
  input: RecallInput,
): Promise<unknown> {
  const model = env.AI_MODEL || DEFAULT_AI_MODEL
  const primer = resultOrThrow(await brain.describePrimer(), 'recall primer failed')
  let usage = { ...EMPTY_USAGE }
  let planned: KipOperation[] = []
  let plannerWarning: string | undefined

  try {
    const plan = await createRecallPlan(env.AI, model, recallPlanMessages(primer, input))
    usage = addUsage(usage, plan.usage)
    planned = keepReadonlyOperations(plan.value.commands.map((command) => ({ command })))
  } catch (error) {
    plannerWarning = error instanceof Error ? error.message : String(error)
  }

  // The grounding lookup runs first and always. It is deterministic, so its
  // failure is the service's failure — a planned read failing is the model's,
  // and answering "nothing found" because one speculative query was malformed
  // would report a miss the memory never had.
  const lookup = conceptLookupCommand(input.query, 8)
  const operations = [lookup, ...planned].slice(0, MAX_KIP_OPERATIONS)
  const results = await brain.executeKipReadonlyBatch(operations)
  const grounding = results[0]
  if (grounding === undefined || grounding.status === 'failed') {
    throw new OperationError('recall KIP failed', 422, grounding?.error)
  }

  const planFailures = results
    .slice(1)
    .flatMap((result) => (result.status === 'failed' ? [result.error?.code ?? 'Unknown'] : []))
  const memories = collectCitations(
    results.slice(1),
    citationsFromLookup(grounding?.result),
  )
  const answer = await createRecallAnswer(
    env.AI,
    model,
    recallAnswerMessages(input, results),
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
}

export async function maintainMemory(
  env: Env,
  brain: BrainRpc,
  input: MaintenanceInput,
): Promise<unknown> {
  const timestamp = input.timestamp ?? new Date().toISOString()
  // Deterministic first, model second — the same order `anda_brain` runs in.
  // Disuse metabolism, silence-Watch expiry and the Skill lifecycle are
  // arithmetic; doing them before the completion means the cycle assesses an
  // already-settled graph rather than one it would have had to settle by hand.
  const settlement = await brain.settleMemory(Date.parse(timestamp) || Date.now())
  const [snapshot, assessment] = await Promise.all([
    brain.executeKipReadonlyBatch(MAINTENANCE_SNAPSHOT),
    brain.maintenanceAssessment(),
  ])
  throwOnKipError(snapshot, 'maintenance snapshot failed')
  const model = env.AI_MODEL || DEFAULT_AI_MODEL
  const plan = await createMutationPlan(
    env.AI,
    model,
    maintenanceMessages(input, { snapshot, assessment }, timestamp),
  )

  const operations = plan.value.commands.map((command) => ({ command }))
  try {
    if (operations.length > 0) assertMaintenanceOperations(operations)
  } catch (error) {
    throw new OperationError(
      error instanceof Error ? error.message : 'invalid maintenance plan',
      422,
    )
  }

  const vocabulary = await declareVocabulary(brain, plan.value)

  const results = operations.length ? await brain.executeMaintenancePlan(operations) : []
  throwOnKipError(results, 'maintenance KIP failed')

  return {
    content: plan.value.summary || 'No maintenance changes were needed.',
    scope: input.scope ?? 'daydream',
    changed: countChanges(results),
    commands: operations.length,
    settlement,
    ...(vocabulary ? { vocabulary } : {}),
    usage: plan.usage,
  }
}

/**
 * Publishes the symbols a plan asked for, when it asked for any.
 *
 * A refusal is not fatal: the plan's other commands are still writable, and the
 * one that needed the refused symbol will fail on its own with an error naming
 * the symbol — which is a better message than a blanket rejection here.
 */
async function declareVocabulary(
  brain: BrainRpc,
  plan: { types: string[]; predicates: string[] },
): Promise<DeclaredVocabulary | undefined> {
  if (plan.types.length === 0 && plan.predicates.length === 0) return undefined
  return brain.declareSymbols(plan.types, plan.predicates)
}

function resultOrThrow(result: KipResult, message: string): unknown {
  if (result.status === 'failed') throw new OperationError(message, 422, result.error)
  return result.result
}

function throwOnKipError(results: readonly KipResult[], message: string): void {
  const failure = firstKipError(results)
  if (failure) throw new OperationError(message, 422, failure)
}
