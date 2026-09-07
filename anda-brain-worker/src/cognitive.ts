/** Trusted host mechanics for CognitiveMemory 2.1; no authority comes from a plan. */
import { contentDigest, tryParseElementId, type Json, type JsonMap } from '@ldclabs/kip-do'

export const BRAIN_CAPABILITIES = {
  kip: '2.0',
  cognitive_memory_schema: '2.1.0',
  memory_interface: false,
  memory_bundles: [],
  learning_scheduler: false,
  external_dispatch: false,
  structured_watch_runtime: true,
  text_watch_evaluator: false,
  procedural_standing: 'unproven',
} satisfies JsonMap

export interface RuntimeOperation {
  operation: 'arm_watch' | 'lease_task'
  target_ref: string
  expected_version: number
}

export function runtimeOperations(value: unknown): RuntimeOperation[] {
  if (value === undefined || value === null) return []
  if (!Array.isArray(value) || value.length > 4) throw new Error('runtime must contain at most 4 operations')
  return value.map((entry) => {
    if (entry === null || typeof entry !== 'object' ||
        !['arm_watch', 'lease_task'].includes(entry.operation) ||
        typeof entry.target_ref !== 'string' ||
        tryParseElementId(entry.target_ref)?.kind !== 'Concept' ||
        !Number.isSafeInteger(entry.expected_version) || entry.expected_version < 1) {
      throw new Error('invalid memory runtime operation')
    }
    return { operation: entry.operation, target_ref: entry.target_ref, expected_version: entry.expected_version }
  })
}

/** Bind hashes to complete parameter positions; Nexus verifies revision content. */
export function digestParameters(value: unknown): JsonMap {
  if (value === undefined || value === null) return {}
  if (typeof value !== 'object' || Array.isArray(value) || Object.keys(value).length > 4 ||
      new TextEncoder().encode(JSON.stringify(value)).length > 65_536) throw new Error('digests must be a bounded object')
  return Object.fromEntries(Object.entries(value).map(([key, content]) => {
    if (!/^digest_[A-Za-z0-9_]{1,56}$/.test(key)) throw new Error('digest binding names must start with digest_')
    return [key, contentDigest(content as Json)]
  }))
}
