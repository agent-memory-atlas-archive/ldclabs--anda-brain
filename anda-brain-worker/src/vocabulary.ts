import { DRAFT_PACKAGE_ID, DRAFT_PACKAGE_REF, lockFromJson, symbols } from '@ldclabs/kip-do/schema'
/**
 * The vocabulary this Space's memories are written against.
 *
 * A memory system meets vocabulary it was not designed with. A conversation
 * introduces `ships_to`; the Cognitive Memory Profile ships `prefers`,
 * `caused_by` and `same_as` and nothing else. KIP 1.x let a write register the
 * missing symbol on the spot, as a `$PropositionType` node in the same graph as
 * the facts — so an ordinary write could change what a type meant, and a model
 * under prompt injection could change it on purpose.
 *
 * KIP 2.0 will not do that. Authoritative Schema is an immutable, versioned
 * Package resolved through the Space's Schema Environment. What a Space may
 * add is its **draft vocabulary** (Spec §20.16): `DEFINE PREDICATE` /
 * `DEFINE CONCEPT TYPE` add one symbol to the Space-local
 * `kip://local/draft@0.0.0`, only ever adding, under `propose_schema`, which
 * grants nothing over existing Schema. A draft stays a draft until the owner
 * promotes it onto an installed package's symbol.
 *
 * Formation drafts from its own `DEFINE` commands, bounded by the formation
 * gate, and from the plan's `types` / `predicates` names, which the host drafts
 * with a generic description. Either way the host caps the Space's vocabulary
 * and queues one `review_schema` SleepTask per new symbol, keyed
 * `review_schema:<kind>:<ref>`.
 *
 * Spaces that grew vocabulary before the draft package existed keep their host
 * package `kip://anda-brain/memory@1.0.N` in force; nothing is added to it any
 * more. Mirrors `anda_brain/src/vocabulary.rs`.
 */

import {
  COGNITIVE_MEMORY,
  CORE_PACKAGE,
  CORE_PACKAGE_REF,
  parseKip,
  type CognitiveNexus,
  type JsonMap,
  type KipResult,
  type SchemaPackage,
} from '@ldclabs/kip-do'
import type { DeclaredVocabulary } from './types.js'

/**
 * The package id this brain published its vocabulary under before the draft
 * vocabulary existed. Read-only now.
 */
const MEMORY_PACKAGE_ID = 'kip://anda-brain/memory'

/**
 * Cap on how many symbols one Space's own vocabulary may hold: its drafts and
 * its legacy host package together. Draft symbols are never removed, so past
 * the cap a new symbol is refused and the answer is to reuse what the Space
 * has. The Rust service enforces the same number.
 */
export const MAX_SYMBOLS = 512

/** Longest symbol name the brain will define. */
const MAX_SYMBOL_CHARS = 64

export type SymbolKind = 'ConceptType' | 'PredicateType'

/** The vocabulary of one Space. */
export class MemoryVocabulary {
  /** Concept types in the legacy host package. */
  readonly types = new Set<string>()
  /** Predicates in the legacy host package. */
  readonly predicates = new Set<string>()
  /** Concept types this Space drafted. */
  readonly draftTypes = new Set<string>()
  /** Predicates this Space drafted. */
  readonly draftPredicates = new Set<string>()
  /** The legacy host package's patch version in force; `0` when none. */
  revision = 0

  /** Symbols other active packages declare, chiefly the Profile's. */
  private readonly borrowedTypes = new Set<string>()
  private readonly borrowedPredicates = new Set<string>()

  /**
   * Reads the vocabulary out of the Space's Schema Environment, which is the
   * only store: a second copy could only disagree with it.
   */
  static load(nexus: CognitiveNexus): MemoryVocabulary {
    const vocabulary = new MemoryVocabulary()
    const stored = nexus.store.schemaEnv(nexus.space)
    const lock = stored ? lockFromJson(stored.lock) : undefined
    // Exactly the lock's packages, each read by reference: the draft package
    // is synthesized from the lock itself and never installed.
    for (const [id, version] of Object.entries(lock?.packages ?? { 'kip://core': '2.0.0' })) {
      if (id === DRAFT_PACKAGE_ID) {
        for (const name of Object.keys(lock?.draft?.concept_types ?? {})) vocabulary.draftTypes.add(name)
        for (const name of Object.keys(lock?.draft?.predicates ?? {})) vocabulary.draftPredicates.add(name)
        continue
      }
      const ref = `${id}@${version}`
      const artifact = ref === CORE_PACKAGE_REF ? CORE_PACKAGE : nexus.store.packageByRef(ref)?.artifact as SchemaPackage | undefined
      if (!artifact) throw new Error(`active vocabulary package missing: ${ref}`)
      const legacy = id === MEMORY_PACKAGE_ID
      for (const [kind, owned, borrowed] of [
        ['ConceptType', vocabulary.types, vocabulary.borrowedTypes],
        ['PredicateType', vocabulary.predicates, vocabulary.borrowedPredicates],
      ] as const) {
        for (const name of symbols(artifact, kind)) (legacy ? owned : borrowed).add(name)
      }
      if (legacy) vocabulary.revision = Number(version.split('.')[2])
    }
    return vocabulary
  }

  /** The legacy host package's reference — `kip://anda-brain/memory@1.0.7`. */
  packageRef(): string {
    return `${MEMORY_PACKAGE_ID}@1.0.${this.revision}`
  }

  /** This Space's own symbols, drafts and legacy together: what the cap counts. */
  get size(): number {
    return this.types.size + this.predicates.size + this.draftTypes.size + this.draftPredicates.size
  }

  /** Whether the Space already resolves this name, whoever defines it. */
  has(kind: SymbolKind, name: string): boolean {
    return kind === 'ConceptType'
      ? this.types.has(name) || this.draftTypes.has(name) || this.borrowedTypes.has(name)
      : this.predicates.has(name) || this.draftPredicates.has(name) || this.borrowedPredicates.has(name)
  }

  /** This vocabulary as the API reports it, sorted so callers can diff it. */
  declared(defined: string[] = [], rejected: string[] = []): DeclaredVocabulary {
    return {
      package_ref: this.revision > 0 ? this.packageRef() : null,
      types: [...this.types].sort(),
      predicates: [...this.predicates].sort(),
      draft_package: DRAFT_PACKAGE_REF,
      draft_types: [...this.draftTypes].sort(),
      draft_predicates: [...this.draftPredicates].sort(),
      defined,
      rejected,
    }
  }
}

/** Runs one host command against the Space. */
export type RunKip = (command: string, parameters: JsonMap) => KipResult

/** The description the host gives a symbol it drafts from a bare name. */
function hostDescription(kind: SymbolKind, name: string): string {
  return kind === 'ConceptType'
    ? `Entity type \`${name}\`, met while forming this Space's memory. It means whatever the ` +
      'sources it came from meant by it; nothing here verifies that.'
    : `Relation \`${name}\`, met while forming this Space's memory. A claim under it is ` +
      'somebody’s statement, never a verified fact.'
}

/**
 * Drafts the names the Space cannot yet resolve (Spec §20.16): one `DEFINE`
 * each, with unconstrained endpoints and open attributes, because these names
 * come from prose. A malformed name or one past {@link MAX_SYMBOLS} is refused
 * on its own; a name defined meanwhile (`SchemaSymbolConflict`) already
 * resolves. Every new symbol queues its review.
 */
export function draftSymbols(
  run: RunKip,
  vocabulary: MemoryVocabulary,
  types: readonly string[],
  predicates: readonly string[],
): { defined: string[]; rejected: string[] } {
  const defined: string[] = []
  const rejected: string[] = []
  let size = vocabulary.size
  for (const [kind, names] of [['ConceptType', types], ['PredicateType', predicates]] as const) {
    for (const name of new Set(names)) {
      if (vocabulary.has(kind, name)) continue
      if (!isSymbolName(kind, name) || size >= MAX_SYMBOLS) {
        rejected.push(name)
        continue
      }
      const description = hostDescription(kind, name)
      const result = run(
        kind === 'ConceptType'
          ? 'DEFINE CONCEPT TYPE :name {description: :description}'
          : 'DEFINE PREDICATE :name {description: :description}',
        { name, description },
      )
      if (result.error?.code === 'SchemaSymbolConflict') continue
      const ref = definedRef(result)
      if (ref === undefined) throw new Error(`defining ${kind} \`${name}\` failed: ${result.error?.message ?? result.status}`)
      size += 1
      queueSchemaReview(run, kind, ref, description)
      defined.push(ref)
    }
  }
  return { defined, rejected }
}

/** The exact reference a committed `DEFINE` answered with. */
export function definedRef(result: KipResult): string | undefined {
  if (result.status === 'failed') return undefined
  const answer = result.result ?? (result.extensions?.['kip-do/outcome'] as { result?: unknown } | undefined)?.result
  const ref = typeof answer === 'object' && answer !== null && !Array.isArray(answer)
    ? (answer as JsonMap).ref
    : undefined
  return typeof ref === 'string' ? ref : undefined
}

/** The kind and description a `DEFINE` command drafts, for its review. */
export function defineOf(command: string): { kind: SymbolKind; description: string } | undefined {
  const parsed = parseKip(command)
  if (!('Kml' in parsed) || parsed.Kml.clauses.length !== 1) return undefined
  const clause = parsed.Kml.clauses[0]!
  if (!('Define' in clause)) return undefined
  const description = (clause.Define.definition as Record<string, unknown>).description
  const literal = typeof description === 'object' && description !== null && 'Value' in description
    ? (description as { Value: unknown }).Value : undefined
  return {
    kind: clause.Define.kind === 'ConceptType' ? 'ConceptType' : 'PredicateType',
    description: typeof literal === 'object' && literal !== null && 'String' in literal
      ? String((literal as { String: unknown }).String) : '',
  }
}

/**
 * Queues the review of one draft symbol: a `review_schema` SleepTask keyed
 * `review_schema:<kind>:<exact ref>` (Spec §20.16, Profile §5.9), so a retry
 * resolves to the same task. Maintenance reviews it and may propose a
 * promotion; only the owner performs one.
 */
export function queueSchemaReview(run: RunKip, kind: SymbolKind, ref: string, description: string): void {
  const name = ref.slice(ref.lastIndexOf('/') + 1)
  const result = run(`CREATE CONCEPT ?task {
  TYPE "SleepTask"
  CLIENT KEY :key
  NAME :name
  SET ATTRIBUTES { task_class: "review_schema", summary: :summary, status: "pending", created_at: :now, symbol_kind: :kind, symbol_ref: :ref }
}`, {
    key: `review_schema:${kind}:${ref}`,
    name: `Review draft ${kind} ${name}`,
    summary: [...`Review the draft ${kind} \`${name}\`: ${description}`].slice(0, 1024).join(''),
    now: new Date().toISOString(),
    kind,
    ref,
  })
  if (result.status === 'failed') {
    throw new Error(`queueing the review of ${ref} failed: ${result.error?.message ?? 'unknown error'}`)
  }
}

/**
 * The Profile plus the legacy host package this Space has in force, if any.
 *
 * Exported because the Durable Object has to pass exactly this set on
 * construction: activating only the Profile would *narrow* the environment and
 * every symbol the legacy package declares would stop resolving. The draft
 * package is Space state the engine keeps across every activation.
 */
export function activeSet(vocabulary: SchemaPackage | null): SchemaPackage[] {
  return vocabulary === null ? [COGNITIVE_MEMORY] : [COGNITIVE_MEMORY, vocabulary]
}

/**
 * The legacy host package this Space has in force, if any. Read from the active
 * lock rather than from the newest installed artifact, so a publication that
 * installed and never activated does not come into force by itself.
 */
export function activeVocabulary(nexus: CognitiveNexus): SchemaPackage | null {
  const stored = nexus.store.schemaEnv(nexus.space)
  const version = stored ? lockFromJson(stored.lock).packages[MEMORY_PACKAGE_ID] : undefined
  if (version === undefined || version === '') return null
  const row = nexus.store.packageByRef(`${MEMORY_PACKAGE_ID}@${version}`)
  return (row?.artifact as SchemaPackage | undefined) ?? null
}

/**
 * The Core element kinds, which a symbol may not shadow (§20.13). Mirrors
 * `anda_kip::CORE_ELEMENT_KINDS`, spelled here because `@ldclabs/kip-do` does
 * not re-export it from its root.
 */
const CORE_ELEMENT_KINDS: readonly string[] = [
  'Concept',
  'Proposition',
  'Assertion',
  'Evidence',
  'Activity',
]

/**
 * Whether a name has the shape this brain gives a symbol of this kind: Concept
 * types UpperCamelCase letters and digits, predicates snake_case. `drug`,
 * `Drug` and `medical device` would otherwise be three symbols meaning one
 * thing, and a draft symbol is never removed.
 */
export function isSymbolName(kind: SymbolKind, name: string): boolean {
  if (name.length === 0 || name.length > MAX_SYMBOL_CHARS) return false
  return kind === 'ConceptType'
    ? /^[A-Z][A-Za-z0-9]*$/.test(name) && !CORE_ELEMENT_KINDS.includes(name)
    : /^[a-z][a-z0-9_]*$/.test(name) && !name.endsWith('_')
}
