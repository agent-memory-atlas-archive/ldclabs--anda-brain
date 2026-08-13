import { parseKip, type Command, type KipResponse } from '@ldclabs/kip-do'
import type { MemoryCitation } from './types.js'

export const MAX_KIP_COMMANDS = 4
export const MAX_KIP_COMMAND_BYTES = 256 * 1024

export function assertCommandBatch(commands: string[]): void {
  if (commands.length > MAX_KIP_COMMANDS) {
    throw new Error(`at most ${MAX_KIP_COMMANDS} KIP commands are allowed`)
  }

  let bytes = 0
  for (const command of commands) {
    if (typeof command !== 'string' || command.trim() === '') {
      throw new Error('KIP commands must be non-empty strings')
    }
    bytes += new TextEncoder().encode(command).byteLength
  }
  if (bytes > MAX_KIP_COMMAND_BYTES) {
    throw new Error(`KIP command batch exceeds ${MAX_KIP_COMMAND_BYTES} bytes`)
  }
}

export function assertReadonlyCommands(commands: string[]): void {
  assertCommandBatch(commands)
  for (const source of commands) {
    const command = parseKip(source)
    if ('Kml' in command) {
      throw new Error('read-only KIP accepts only KQL and META commands')
    }
  }
}

export function assertBoundedReadonlyCommands(
  commands: string[],
  maxResults: number,
): void {
  assertCommandBatch(commands)
  for (const source of commands) {
    const command = parseKip(source)
    if ('Kml' in command) {
      throw new Error('read-only KIP accepts only KQL and META commands')
    }
    assertResultLimit(command, maxResults)
  }
}

export function assertFormationCommands(commands: string[]): void {
  assertCommandBatch(commands)
  for (const source of commands) {
    const command = parseKip(source)
    if (!('Kml' in command) || !('Upsert' in command.Kml)) {
      throw new Error('formation accepts only KIP UPSERT commands')
    }
  }
}

export function assertMaintenanceCommands(commands: string[]): void {
  assertCommandBatch(commands)
  for (const source of commands) {
    const command = parseKip(source)
    if (!('Kml' in command)) {
      throw new Error('maintenance plans must contain KML commands')
    }
    if ('Delete' in command.Kml) {
      throw new Error('maintenance plans cannot issue KIP DELETE commands')
    }
    if (
      'Update' in command.Kml &&
      (command.Kml.Update.limit === null || command.Kml.Update.limit > 20)
    ) {
      throw new Error('maintenance UPDATE commands must use LIMIT 20 or less')
    }
  }
}

export function keepReadonlyCommands(commands: string[]): string[] {
  const valid: string[] = []
  for (const command of commands) {
    try {
      assertBoundedReadonlyCommands([command], 20)
      valid.push(command)
    } catch {
      // Model-produced invalid, mutating, or unbounded commands are discarded.
      // The deterministic SEARCH fallback still gives recall useful evidence.
    }
  }
  return valid
}

export function conceptSearchCommand(query: string, limit = 8): string {
  const boundedLimit = Math.max(1, Math.min(20, Math.trunc(limit)))
  return `SEARCH CONCEPT ${JSON.stringify(query)} LIMIT ${boundedLimit}`
}

export function firstKipError(
  responses: KipResponse[],
): Extract<KipResponse, { error: unknown }> | undefined {
  return responses.find(
    (response): response is Extract<KipResponse, { error: unknown }> =>
      'error' in response,
  )
}

export function countWrites(responses: KipResponse[]): {
  concepts: number
  propositions: number
} {
  let concepts = 0
  let propositions = 0
  for (const response of responses) {
    if (!('result' in response) || !isObject(response.result)) continue
    const conceptIds = response.result.upsert_concept_nodes
    const propositionIds = response.result.upsert_proposition_links
    if (Array.isArray(conceptIds)) concepts += conceptIds.length
    if (Array.isArray(propositionIds)) propositions += propositionIds.length
  }
  return { concepts, propositions }
}

export function countChanges(responses: KipResponse[]): {
  total: number
  upserted: number
  updated: number
  deleted: number
  merged: number
} {
  let upserted = 0
  let updated = 0
  let deleted = 0
  let merged = 0

  for (const response of responses) {
    if (!('result' in response) || !isObject(response.result)) continue
    const result = response.result
    upserted += arrayLength(result.upsert_concept_nodes)
    upserted += arrayLength(result.upsert_proposition_links)
    updated += count(result.updated)
    updated += count(result.updated_concepts)
    updated += count(result.updated_propositions)
    deleted += count(result.deleted_concepts)
    deleted += count(result.deleted_propositions)
    if (result.merged === true) merged += 1
  }

  return {
    total: upserted + updated + deleted + merged,
    upserted,
    updated,
    deleted,
    merged,
  }
}

export function collectCitations(responses: KipResponse[]): MemoryCitation[] {
  const citations = new Map<string, MemoryCitation>()
  for (const response of responses) {
    if ('result' in response) visit(response.result, citations)
  }
  return [...citations.values()].slice(0, 16)
}

function visit(value: unknown, citations: Map<string, MemoryCitation>): void {
  if (citations.size >= 16) return
  if (Array.isArray(value)) {
    for (const item of value) visit(item, citations)
    return
  }
  if (!isObject(value)) return

  const id = typeof value.id === 'string' ? value.id : undefined
  if (id && /^(?:C|P):/.test(id)) {
    const metadata = isObject(value.metadata) ? value.metadata : undefined
    const citation: MemoryCitation = { entity: id }
    if (typeof value.type === 'string') citation.type = value.type
    else if (typeof value.predicate === 'string') citation.type = value.predicate
    if (typeof value.name === 'string') citation.name = value.name
    if (typeof metadata?.confidence === 'number') {
      citation.confidence = metadata.confidence
    }
    const source = firstString(metadata?.source)
    if (source) citation.source = source
    if (typeof metadata?.created_at === 'string') {
      citation.created_at = metadata.created_at
    }
    citations.set(id, citation)
  }

  for (const child of Object.values(value)) visit(child, citations)
}

function firstString(value: unknown): string | undefined {
  if (typeof value === 'string') return value
  if (Array.isArray(value)) return value.find((item): item is string => typeof item === 'string')
  return undefined
}

function assertResultLimit(command: Command, maxResults: number): void {
  let limit: number | null | undefined
  if ('Kql' in command) {
    limit = command.Kql.limit
  } else if ('Kml' in command) {
    throw new Error('read-only KIP accepts only KQL and META commands')
  } else if ('Search' in command.Meta) {
    limit = command.Meta.Search.limit
  } else if ('Export' in command.Meta) {
    limit = command.Meta.Export.limit
  } else {
    const describe = command.Meta.Describe
    if (typeof describe === 'object' && 'ConceptTypes' in describe) {
      limit = describe.ConceptTypes.limit
    } else if (typeof describe === 'object' && 'PropositionTypes' in describe) {
      limit = describe.PropositionTypes.limit
    } else {
      // Primer, domains, and a single type description are bounded metadata reads.
      return
    }
  }

  if (limit === null || limit === undefined || limit > maxResults) {
    throw new Error(`model read commands must use LIMIT ${maxResults} or less`)
  }
}

function arrayLength(value: unknown): number {
  return Array.isArray(value) ? value.length : 0
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0
    ? Math.trunc(value)
    : 0
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
