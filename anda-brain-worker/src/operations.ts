import type { KipResponse } from '@ldclabs/kip-do'
import {
  DEFAULT_AI_MODEL,
  addUsage,
  createMutationPlan,
  createRecallAnswer,
  createRecallPlan,
} from './ai.js'
import {
  assertFormationCommands,
  assertMaintenanceCommands,
  collectCitations,
  conceptSearchCommand,
  countChanges,
  countWrites,
  firstKipError,
  keepReadonlyCommands,
} from './kip.js'
import {
  formationMessages,
  maintenanceMessages,
  recallAnswerMessages,
  recallPlanMessages,
} from './prompts.js'
import type {
  BrainRpc,
  Env,
  FormationInput,
  MaintenanceInput,
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
): Promise<unknown> {
  const timestamp = input.timestamp ?? new Date().toISOString()
  const primer = resultOrThrow(await brain.describePrimer(), 'formation primer failed')
  const model = env.AI_MODEL || DEFAULT_AI_MODEL
  const plan = await createMutationPlan(
    env.AI,
    model,
    formationMessages(primer, input, timestamp),
  )

  try {
    assertFormationCommands(plan.value.commands)
  } catch (error) {
    throw new OperationError(
      error instanceof Error ? error.message : 'invalid formation plan',
      422,
    )
  }

  const responses = plan.value.commands.length
    ? await brain.executeFormationPlan(plan.value.commands)
    : []
  throwOnKipError(responses, 'formation KIP failed')

  return {
    content: plan.value.summary || 'No durable memory was extracted.',
    stored: countWrites(responses),
    commands: plan.value.commands.length,
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
  let plannedCommands: string[] = []
  let plannerWarning: string | undefined

  try {
    const plan = await createRecallPlan(
      env.AI,
      model,
      recallPlanMessages(primer, input),
    )
    usage = addUsage(usage, plan.usage)
    plannedCommands = keepReadonlyCommands(plan.value.commands)
  } catch (error) {
    plannerWarning = error instanceof Error ? error.message : String(error)
  }

  const fallback = conceptSearchCommand(input.query, 8)
  const commands = [...new Set([fallback, ...plannedCommands])].slice(0, 4)
  const evidence = await brain.executeKipReadonlyBatch(commands)
  throwOnKipError(evidence, 'recall KIP failed')
  const answer = await createRecallAnswer(
    env.AI,
    model,
    recallAnswerMessages(input, evidence),
  )
  usage = addUsage(usage, answer.usage)
  const memories = collectCitations(evidence)

  return {
    content: answer.value.answer,
    answer: answer.value.answer,
    found: answer.value.found && memories.length > 0,
    uncertainty: answer.value.uncertainty,
    memories,
    usage,
    diagnostics: {
      kip_commands: commands.length,
      ...(plannerWarning ? { planner_warning: plannerWarning } : {}),
    },
  }
}

export async function maintainMemory(
  env: Env,
  brain: BrainRpc,
  input: MaintenanceInput,
): Promise<unknown> {
  const timestamp = input.timestamp ?? new Date().toISOString()
  const snapshotCommands = [
    'FIND(?event) WHERE { ?event {type: "Event"} } ORDER BY ?event.metadata._updated_at DESC LIMIT 20',
    'FIND(?task) WHERE { ?task {type: "SleepTask"} } LIMIT 20',
  ]
  const snapshot = await brain.executeKipReadonlyBatch(snapshotCommands)
  throwOnKipError(snapshot, 'maintenance snapshot failed')
  const model = env.AI_MODEL || DEFAULT_AI_MODEL
  const plan = await createMutationPlan(
    env.AI,
    model,
    maintenanceMessages(input, snapshot, timestamp),
  )
  try {
    assertMaintenanceCommands(plan.value.commands)
  } catch (error) {
    throw new OperationError(
      error instanceof Error ? error.message : 'invalid maintenance plan',
      422,
    )
  }
  const responses = plan.value.commands.length
    ? await brain.executeMaintenancePlan(plan.value.commands)
    : []
  throwOnKipError(responses, 'maintenance KIP failed')

  return {
    content: plan.value.summary || 'No maintenance changes were needed.',
    scope: input.scope ?? 'daydream',
    changed: countChanges(responses),
    commands: plan.value.commands.length,
    usage: plan.usage,
  }
}

function resultOrThrow(response: KipResponse, message: string): unknown {
  if ('result' in response) return response.result
  throw new OperationError(message, 422, response.error)
}

function throwOnKipError(responses: KipResponse[], message: string): void {
  const failure = firstKipError(responses)
  if (failure) throw new OperationError(message, 422, failure.error)
}
