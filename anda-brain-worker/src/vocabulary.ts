import { lockFromJson, symbols } from '@ldclabs/kip-do/schema'
/**
 * The Schema Package this Space's own memories are written against.
 *
 * A memory system meets vocabulary it was not designed with. A conversation
 * introduces `ships_to`; the Cognitive Memory Profile ships `prefers`,
 * `caused_by` and `same_as` and nothing else. KIP 1.x let a write register the
 * missing symbol on the spot, as a `$PropositionType` node in the same graph as
 * the facts — so an ordinary write could change what a type meant, and a model
 * under prompt injection could change it on purpose.
 *
 * KIP 2.0 will not do that. Authoritative Schema is an immutable, versioned
 * Package resolved through the Space's Schema Environment, and KML cannot touch
 * it. So new vocabulary enters through the **host**: this Durable Object keeps
 * one package, publishes a new version when a symbol is genuinely new, and
 * activates it alongside the Profile.
 *
 * The model still proposes the words — as two fields of its plan, which the
 * host validates, caps and versions. What that buys is concrete: every
 * element's `schema_ref` names an exact version forever, so a memory's meaning
 * cannot drift underneath it.
 *
 * The Schema Environment is also the *only* store. The vocabulary is read back
 * from the active lock and its exact packages rather than kept in a second place
 * that could disagree with it.
 */

import {
  COGNITIVE_MEMORY,
  CORE_PACKAGE,
  CORE_PACKAGE_REF,
  type CognitiveNexus,
  type Json,
  type SchemaPackage,
} from '@ldclabs/kip-do'
import type { DeclaredVocabulary } from './types.js'

/**
 * The package id this brain publishes its own vocabulary under.
 *
 * Deliberately outside `kip://core` and `kip://profiles`: those namespaces
 * carry meanings other engines are expected to share, and a predicate one
 * conversation happened to use is not one of them.
 */
const MEMORY_PACKAGE_ID = 'kip://anda-brain/memory'

/**
 * Cap on how many symbols one Space's vocabulary may hold.
 *
 * The package is re-published on every extension, so an unbounded vocabulary
 * would grow both the artifact and the environment's history without limit.
 * Past the cap the brain refuses the new symbol rather than the whole write: a
 * Space that has introduced this many distinct predicates is accumulating
 * synonyms, and the answer is to reuse what it has.
 */
const MAX_SYMBOLS = 512

/** Longest symbol name the brain will publish. */
const MAX_SYMBOL_CHARS = 64

/** The vocabulary of one Space. */
export class MemoryVocabulary {
  /** Concept types this brain published, as UpperCamelCase local names. */
  readonly types = new Set<string>()

  /** Predicates this brain published, as snake_case local names. */
  readonly predicates = new Set<string>()

  /**
   * How many times the package has been published — the version's patch
   * component. A counter rather than a content hash: a version must never go
   * backwards, and two vocabularies differing only in arrival order are still
   * two published artifacts in the environment's history.
   *
   * This is the revision **in force**, so `packageRef()` names the artifact
   * that actually declares these symbols.
   */
  revision = 0

  /**
   * The highest revision this Space has ever installed, in force or not.
   *
   * Publishing installs the artifact and then activates it, and a failure
   * between those two leaves a version installed that never came into force.
   * Reusing that number with a different symbol set would be refused for a
   * digest mismatch — a package version identifies one canonical content
   * forever — and the Space could then never grow its vocabulary again.
   * Skipping the stranded number costs one integer.
   */
  private floor = 0

  /**
   * Symbols other active packages declare, chiefly the Profile's. Writable, but
   * not this package's to redeclare.
   */
  private readonly borrowedTypes = new Set<string>()
  private readonly borrowedPredicates = new Set<string>()

  /**
   * Reads the vocabulary out of the Space's Schema Environment.
   *
   * Keeping a second copy beside it would create exactly one bug — the two
   * disagreeing — and no capability.
   */
  static load(nexus: CognitiveNexus, reserveVersion = false): MemoryVocabulary {
    const vocabulary = new MemoryVocabulary()
    const stored = nexus.store.schemaEnv(nexus.space)
    const packages = stored ? lockFromJson(stored.lock).packages : { 'kip://core': '2.0.0' }
    for (const [id, version] of Object.entries(packages)) {
      const ref = `${id}@${version}`
      const artifact = ref === CORE_PACKAGE_REF ? CORE_PACKAGE : nexus.store.packageByRef(ref)?.artifact as SchemaPackage | undefined
      if (!artifact) throw new Error(`active vocabulary package missing: ${ref}`)
      const mine = id === MEMORY_PACKAGE_ID
      for (const [kind, owned, borrowed] of [
        ['ConceptType', vocabulary.types, vocabulary.borrowedTypes],
        ['PredicateType', vocabulary.predicates, vocabulary.borrowedPredicates],
      ] as const) {
        for (const name of symbols(artifact, kind)) (mine ? owned : borrowed).add(name)
      }
      if (mine) vocabulary.revision = Number(version.split('.')[2])
    }
    if (reserveVersion) vocabulary.floor = highestInstalledRevision(nexus)
    return vocabulary
  }

  /** The exact version this vocabulary is published under. */
  version(): string {
    return `1.0.${this.revision}`
  }

  /** The package reference — `kip://anda-brain/memory@1.0.7`. */
  packageRef(): string {
    return `${MEMORY_PACKAGE_ID}@${this.version()}`
  }

  /** How many symbols this package declares. */
  get size(): number {
    return this.types.size + this.predicates.size
  }

  /**
   * This vocabulary as the API reports it.
   *
   * Sorted, because a caller diffing two of these should see what changed
   * rather than what arrived first.
   */
  declared(rejected: string[] = []): DeclaredVocabulary {
    return {
      package_ref: this.packageRef(),
      types: [...this.types].sort(),
      predicates: [...this.predicates].sort(),
      rejected,
    }
  }

  /**
   * Adds symbols, bumping the revision when anything was genuinely new.
   *
   * Returns the names it refused: malformed, or past {@link MAX_SYMBOLS}. A
   * symbol another active package already declares is *not* refused and not
   * added — redeclaring it would make every reference to it ambiguous, and the
   * Profile's meaning is the better one anyway.
   */
  extend(types: Iterable<string>, predicates: Iterable<string>): string[] {
    const rejected: string[] = []
    let changed = false
    for (const [names, valid, mine, borrowed] of [
      [types, isTypeName, this.types, this.borrowedTypes],
      [predicates, isPredicateName, this.predicates, this.borrowedPredicates],
    ] as const) {
      for (const name of names) {
        if (borrowed.has(name) || mine.has(name)) continue
        if (!valid(name) || this.size >= MAX_SYMBOLS) {
          rejected.push(name)
          continue
        }
        mine.add(name)
        changed = true
      }
    }
    if (changed) {
      this.revision = Math.max(this.revision, this.floor) + 1
      this.floor = this.revision
    }
    return rejected
  }

  /**
   * Renders the Schema Package artifact.
   *
   * Every predicate accepts any Concept on both ends and is declared
   * `open_world` and non-`functional`. That is not laziness: these symbols come
   * from prose, so nothing here has a basis for claiming a relation holds
   * between exactly two types, or that what was written down was everything —
   * and a schema asserting either would turn a gap in what the brain was told
   * into a closed world.
   */
  artifact(): SchemaPackage {
    const packageRef = this.packageRef()
    const concept_types: Record<string, Json> = {}
    for (const name of [...this.types].sort()) {
      concept_types[name] = {
        ref: `${packageRef}/${name}`,
        kind: 'ConceptType',
        description:
          `Entity type \`${name}\`, met while forming this Space's memory. It means ` +
          'whatever the sources it came from meant by it; nothing here verifies that.',
        attributes: { open: true, fields: {} },
      }
    }
    const predicates: Record<string, Json> = {}
    for (const name of [...this.predicates].sort()) {
      predicates[name] = {
        ref: `${packageRef}/${name}`,
        kind: 'PredicateType',
        description:
          `Relation \`${name}\`, met while forming this Space's memory. A claim under ` +
          'it is somebody’s statement, never a verified fact.',
        subject: { kinds: ['Concept'] },
        object: { kinds: ['Concept'] },
        functional: false,
        open_world: true,
        complete: false,
      }
    }

    return {
      format: 'KIP-Schema-Package',
      format_version: '2.0-draft',
      manifest: {
        package_id: MEMORY_PACKAGE_ID,
        version: this.version(),
        package_ref: packageRef,
        name: 'Anda Brain space vocabulary',
        description:
          "Concept types and predicates this Space's memory needed beyond the Cognitive " +
          'Memory Profile. Deployment-local: these symbols mean what their sources meant ' +
          'and are not portable ontology.',
        publisher: 'urn:kip:publisher:anda-brain',
        purpose: 'deployment_extension',
        stability: 'experimental',
        executable: false,
      },
      dependencies: [
        {
          package_id: 'kip://core',
          version: '2.0.0',
          package_ref: 'kip://core@2.0.0',
          required: true,
        },
      ],
      definitions: { concept_types, predicates } as SchemaPackage['definitions'],
      model_hints: {
        provenance_invariant:
          'A symbol here was proposed by a model reading a conversation. Its presence is ' +
          'not evidence that the relation it names is real.',
      },
    }
  }

  /**
   * Publishes this vocabulary and puts it in force alongside the Profile.
   *
   * `activatePackages` re-activates only when the resulting lock differs from
   * the one already in force, so calling this with an unchanged vocabulary does
   * not walk the environment version forward.
   */
  activate(nexus: CognitiveNexus): void {
    nexus.activatePackages(activeSet(this.size > 0 ? this.artifact() : null))
  }
}

/**
 * The packages a Space runs with: the Profile, plus its own vocabulary.
 *
 * Exported because the Durable Object has to pass exactly this set on
 * construction. A restart that activated only the Profile would *narrow* the
 * environment, and every local name this Space had published would stop
 * resolving until something declared it again.
 */
export function activeSet(vocabulary: SchemaPackage | null): SchemaPackage[] {
  return vocabulary === null ? [COGNITIVE_MEMORY] : [COGNITIVE_MEMORY, vocabulary]
}

/**
 * The highest revision this Space has ever *installed*, active or not.
 *
 * Read from the installed packages rather than from the active environment,
 * because a version number must never be reused. Publishing installs the
 * artifact and then activates it, and a failure between those two leaves a
 * version installed that never came into force — if the next publication reused
 * that number with a different symbol set, the install would be refused for a
 * digest mismatch and the Space could never grow its vocabulary again. Skipping
 * the stranded number costs one integer.
 */
function highestInstalledRevision(nexus: CognitiveNexus): number {
  let revision = 0
  for (const row of nexus.store.sql.exec<{version: string}>(
    'SELECT version FROM schema_packages WHERE package_id = ?', MEMORY_PACKAGE_ID,
  )) {
    const patch = Number(row.version.split('.')[2] ?? 0)
    if (Number.isInteger(patch)) revision = Math.max(revision, patch)
  }
  return revision
}

/**
 * The vocabulary artifact this Space currently has in force, if any.
 *
 * Read from the active lock rather than from the newest installed artifact: a
 * publication that installed and then failed to activate must not come into
 * force by itself on the next restart.
 */
export function activeVocabulary(nexus: CognitiveNexus): SchemaPackage | null {
  const stored = nexus.store.schemaEnv(nexus.space)
  const version = stored ? lockFromJson(stored.lock).packages[MEMORY_PACKAGE_ID] : undefined
  if (version === undefined || version === '') return null
  const row = nexus.store.packageByRef(`${MEMORY_PACKAGE_ID}@${version}`)
  return (row?.artifact as SchemaPackage | undefined) ?? null
}

/**
 * The Core element kinds, which a package may not shadow (§20.13).
 *
 * Mirrors `anda_kip::CORE_ELEMENT_KINDS`, spelled here because `@ldclabs/kip-do`
 * does not re-export it from its root. Five names fixed by the Core package;
 * they do not move.
 */
const CORE_ELEMENT_KINDS: readonly string[] = [
  'Concept',
  'Proposition',
  'Assertion',
  'Evidence',
  'Activity',
]

/**
 * A Concept type name: UpperCamelCase, alphanumeric.
 *
 * Enforced here rather than left to the model, because `drug`, `Drug` and
 * `medical device` would otherwise become three types meaning one thing — and,
 * unlike a 1.x graph node, a published symbol cannot be tidied away.
 */
function isTypeName(name: string): boolean {
  return (
    name.length > 0 &&
    name.length <= MAX_SYMBOL_CHARS &&
    /^[A-Z][A-Za-z0-9]*$/.test(name) &&
    // Core exports the element kinds and no Concept types at all, so
    // `LIST TYPES` never reports them and the borrowed set cannot catch this.
    // A model proposing `Assertion` would pass every other check and be refused
    // at *package installation*, taking the whole publish down and leaving the
    // Space unable to grow its vocabulary again. Refused here, where the answer
    // is one rejected name.
    !CORE_ELEMENT_KINDS.includes(name)
  )
}

/** A predicate name: snake_case, starting with a lowercase letter. */
function isPredicateName(name: string): boolean {
  return (
    name.length > 0 &&
    name.length <= MAX_SYMBOL_CHARS &&
    /^[a-z][a-z0-9_]*$/.test(name) &&
    !name.endsWith('_')
  )
}
