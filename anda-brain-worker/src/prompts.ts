/**
 * The three mode prompts.
 *
 * Each is the KIP 2.0 reference Brain policy for that mode plus this
 * deployment's contract (section A of the corresponding `assets/Brain*.md`),
 * followed by the language the policy assumes is loaded. KIP 1.x taught the
 * language from a short guide written here; a hand-maintained summary of a
 * protocol drifts, and the copy that drifts silently is the one a model then
 * writes against.
 *
 * The payload goes last so head/tail truncation keeps the newest turns.
 */

import { BRAIN_CAPABILITIES } from './cognitive.js'
import type { AiMessage } from './ai.js'
import {
  BRAIN_FORMATION,
  BRAIN_MAINTENANCE,
  BRAIN_RECALL,
  COGNITIVE_MEMORY_PROFILE,
  KIP_FORMATION_CARD,
  KIP_RECALL_CARD,
  KIP_MAINTENANCE_CARD,
  KIP_SYNTAX,
} from './assets.generated.js'
import type { FormationInput, MaintenanceInput, RecallInput } from './types.js'

/**
 * Load the role-specific card with the ontology and actual adapter limits.
 * Planning stages retain the full syntax card for ordinary single-pass plans.
 * The structured AI runner can serve bounded reference requests for details.
 */
const reference = (card: string, syntax = false): string =>
  `${card}\n\n${syntax ? `${KIP_SYNTAX}\n\n` : ''}${COGNITIVE_MEMORY_PROFILE}\n\n` +
  `Brain adapter capabilities (distinct from engine Schema): ${JSON.stringify(BRAIN_CAPABILITIES)}`

export function formationMessages(
  primer: unknown,
  input: FormationInput,
  timestamp: string,
): AiMessage[] {
  return [
    { role: 'system', content: `${BRAIN_FORMATION}\n\n---\n\n${reference(KIP_FORMATION_CARD, true)}` },
    {
      role: 'user',
      content: boundedJson({
        stage: 'formation',
        timestamp,
        ...(input.timestamp && input.timestamp !== timestamp ? { source_timestamp: input.timestamp } : {}),
        context: input.context ?? {},
        primer,
        messages: input.messages,
      }),
    },
  ]
}

export function recallPlanMessages(primer: unknown, input: RecallInput): AiMessage[] {
  return [
    { role: 'system', content: `${BRAIN_RECALL}\n\n---\n\n${reference(KIP_RECALL_CARD, true)}` },
    {
      role: 'user',
      content: boundedJson({
        stage: 'plan',
        query: input.query,
        context: input.context ?? {},
        primer,
      }),
    },
  ]
}

/**
 * The answer stage gets the policy and the ontology, not the syntax card.
 *
 * It writes no commands, and the card is the largest thing in the context: what
 * it needs is §9 BELIEF, §10 Open World and what a `Preference` means, all of
 * which live in the other two documents.
 */
export function recallAnswerMessages(input: RecallInput, evidence: unknown): AiMessage[] {
  return [
    { role: 'system', content: `${BRAIN_RECALL}\n\n---\n\n${reference(KIP_RECALL_CARD)}` },
    {
      role: 'user',
      content: boundedJson({
        stage: 'answer',
        query: input.query,
        context: input.context ?? {},
        evidence,
      }),
    },
  ]
}

export function maintenanceMessages(
  input: MaintenanceInput,
  snapshot: unknown,
  timestamp: string,
): AiMessage[] {
  return [
    { role: 'system', content: `${BRAIN_MAINTENANCE}\n\n---\n\n${reference(`${KIP_RECALL_CARD}\n${KIP_FORMATION_CARD}\n${KIP_MAINTENANCE_CARD}`, true)}` },
    {
      role: 'user',
      content: boundedJson({ stage: 'maintenance', timestamp, request: input, snapshot }),
    },
  ]
}

/**
 * Serializes a payload, keeping its head and tail when it does not fit.
 *
 * The result is always valid JSON: a truncated conversation is described as
 * truncated rather than handed over as a broken document the model then has to
 * guess at.
 */
export function boundedJson(value: unknown, maxChars = 64_000): string {
  const serialized = JSON.stringify(value)
  if (serialized.length <= maxChars) return serialized

  const payload = {
    truncated: true,
    original_chars: serialized.length,
    head: '',
    tail: '',
  }
  let headChars = Math.max(0, Math.floor((maxChars - 160) * 0.4))
  let tailChars = Math.max(0, maxChars - 160 - headChars)

  while (true) {
    payload.head = serialized.slice(0, headChars)
    payload.tail = tailChars === 0 ? '' : serialized.slice(-tailChars)
    const bounded = JSON.stringify(payload)
    if (bounded.length <= maxChars) return bounded

    const excess = bounded.length - maxChars
    if (headChars === 0 && tailChars === 0) return JSON.stringify({ truncated: true })
    const headReduction = Math.min(headChars, Math.ceil(excess * 0.4))
    const tailReduction = Math.min(tailChars, Math.max(1, excess - headReduction))
    headChars -= headReduction
    tailChars -= tailReduction
  }
}
