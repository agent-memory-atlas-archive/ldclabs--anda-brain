import {
  KipDatabase,
  tryParseElementId,
  type JsonMap,
  type KipResult,
  type ReadOptions,
  type RequestContext,
  type SchemaPackage,
} from '@ldclabs/kip-do'
import {
  assertFormationOperations,
  assertMaintenanceOperations,
  assertReadonlyOperations,
  type KipExecution,
  type KipOperation,
} from './kip.js'
import type { BrainStats, DeclaredVocabulary, Env } from './types.js'
import { MemoryVocabulary, activeSet, activeVocabulary } from './vocabulary.js'

const APP_BOOTSTRAP_KEY = '__anda_brain_worker_bootstrap_version'
const APP_BOOTSTRAP_VERSION = '3'

/** The key of the Person this brain speaks as when it asserts something. */
export const SELF_ACTOR_KEY = '$self'

/**
 * The brain's own semantic identity.
 *
 * One Person, keyed rather than named, because a key is immutable identity and
 * a name is a mutable label. KIP 1.x also bootstrapped a `$system` Person and
 * used it as the author of anything the service decided by itself; 2.0 has no
 * place for it. Authority lives in Governance and is held by a Principal —
 * writing it into cognitive content as an actor was the collapse that made
 * "the system says so" look like a claim somebody made.
 */
const ACTOR_BOOTSTRAP = `MUTATE {
  UPSERT CONCEPT ?self {
    MATCH {type: "Person", key: :key}
    SET FIELDS { name: :key }
  }
}`

/** One compact Anda Brain graph per Durable Object / space id. */
export class AndaBrain extends KipDatabase<Env> {
  /**
   * The Profile plus whatever vocabulary this Space has already published.
   *
   * The base class activates the Cognitive Memory Profile and stops there,
   * which is right for a Space that has none of its own. Returning only the
   * Profile here would *narrow* the environment on every restart: the lock
   * would lose this Space's package, every local name it had published would
   * stop resolving, and the environment version would walk forward for a change
   * nobody made.
   */
  protected override packages(): readonly SchemaPackage[] {
    return activeSet(activeVocabulary(this.nexus))
  }

  override executeKip(
    command: string,
    params: JsonMap = {},
    context?: RequestContext,
    read?: ReadOptions,
  ): KipResult {
    this.ensureInitialized()
    return super.executeKip(command, params, context, read)
  }

  /**
   * `execution` is forwarded, not defaulted.
   *
   * The base class decides `sequence` / `on_error` from the request envelope
   * and hands the decision down to this method. Dropping the argument here —
   * which an override that only needed `ensureInitialized` would do by
   * omission — would run a batch the caller asked to stop at the first failure
   * as `independent`, committing writes it asked to have skipped, and the
   * answer would still say `sequence` because the envelope reports what was
   * requested.
   */
  override executeKipBatch(
    operations: readonly KipOperation[],
    context?: RequestContext,
    read?: ReadOptions,
    execution?: KipExecution,
  ): KipResult[] {
    this.ensureInitialized()
    return execution === undefined
      ? super.executeKipBatch(operations, context, read)
      : super.executeKipBatch(operations, context, read, execution)
  }

  /** KQL and META only, decided by what each command parses to. */
  executeKipReadonlyBatch(
    operations: readonly KipOperation[],
    execution?: KipExecution,
  ): KipResult[] {
    assertReadonlyOperations(operations)
    this.ensureInitialized()
    return execution === undefined
      ? super.executeKipBatch(operations)
      : super.executeKipBatch(operations, undefined, undefined, execution)
  }

  executeFormationPlan(operations: readonly KipOperation[]): KipResult[] {
    assertFormationOperations(operations)
    this.ensureInitialized()
    return super.executeKipBatch(operations)
  }

  executeMaintenancePlan(operations: readonly KipOperation[]): KipResult[] {
    assertMaintenanceOperations(operations)
    this.ensureInitialized()
    return super.executeKipBatch(operations)
  }

  describePrimer(): KipResult {
    this.ensureInitialized()
    return super.executeKip('DESCRIBE PRIMER')
  }

  /**
   * Publishes the symbols a plan asked for and puts them in force.
   *
   * This is the whole reason the host is in the loop: KIP 2.0 took schema out
   * of the language a model writes, so a model that needs `ships_to` has to
   * ask. What the host adds on top — name validation, a cap, and a version — is
   * what makes asking better than declaring, because the set of things this
   * Brain can say stays a reviewable artifact rather than whatever its models
   * happened to emit.
   *
   * A refused name is reported, not raised. The caller can still write every
   * memory whose symbols were accepted.
   */
  declareSymbols(types: readonly string[], predicates: readonly string[]): DeclaredVocabulary {
    this.ensureInitialized()
    const vocabulary = MemoryVocabulary.load(this.nexus)
    const before = vocabulary.revision
    const rejected = vocabulary.extend(types, predicates)
    if (vocabulary.revision !== before) vocabulary.activate(this.nexus)
    return {
      package_ref: vocabulary.packageRef(),
      // Sorted, because a caller diffing two of these should see what changed
      // rather than what arrived first.
      types: [...vocabulary.types].sort(),
      predicates: [...vocabulary.predicates].sort(),
      rejected,
    }
  }

  /** What this Space can already say, without publishing anything. */
  vocabulary(): DeclaredVocabulary {
    this.ensureInitialized()
    const vocabulary = MemoryVocabulary.load(this.nexus)
    return {
      package_ref: vocabulary.packageRef(),
      types: [...vocabulary.types].sort(),
      predicates: [...vocabulary.predicates].sort(),
      rejected: [],
    }
  }

  stats(): BrainStats {
    this.ensureInitialized()
    const count = (table: string): number =>
      this.ctx.storage.sql
        .exec<{ count: number }>(`SELECT COUNT(*) AS count FROM ${table}`)
        .one().count

    return {
      concepts: count('concepts'),
      propositions: count('propositions'),
      assertions: count('assertions'),
      evidence: count('evidence'),
      schema_environment_version: this.nexus.environment().version,
      initialized_at: this.ctx.storage.kv.get<string>(`${APP_BOOTSTRAP_KEY}:at`) ?? '',
      engine: '@ldclabs/kip-do',
      kip: '2.0',
    }
  }

  /**
   * Bootstraps once per object.
   *
   * Synchronous throughout, because everything under it is: the 2.0 engine runs
   * a command against the object's own SQLite without awaiting, and a Durable
   * Object is single-threaded — so there is no window for a second caller to
   * observe a half-initialized brain and nothing for a promise to guard.
   */
  private ensureInitialized(): void {
    if (this.ctx.storage.kv.get<string>(APP_BOOTSTRAP_KEY) === APP_BOOTSTRAP_VERSION) {
      return
    }
    const result = super.executeKip(ACTOR_BOOTSTRAP, { key: SELF_ACTOR_KEY })
    if (result.status === 'failed') {
      throw new Error(`failed to initialize the brain's actor: ${result.error?.message}`)
    }
    this.designateSelfConcept()
    this.ctx.storage.kv.put(APP_BOOTSTRAP_KEY, APP_BOOTSTRAP_VERSION)
    this.ctx.storage.kv.put(`${APP_BOOTSTRAP_KEY}:at`, new Date().toISOString())
  }

  /**
   * Points the Space's §5.6 self identity at the `$self` Person just created.
   *
   * `DESCRIBE PRIMER` reports the authenticated Principal and the semantic
   * `$self` as the two different things §64.2 requires it to distinguish, and
   * this service puts that primer in front of every mode. Bootstrapping the
   * Person and then leaving the designation empty put a primer saying this
   * Space has no `$self` in the same context window as a Recall policy opening
   * "you operate on behalf of `$self`, the owner of this MemorySpace".
   *
   * Protected Space configuration, so it goes through the Governance operation
   * rather than KML: §5.6 forbids ordinary KML from creating or changing it,
   * which is what stops cognitive content from deciding who the Brain is.
   *
   * Best-effort: a failure here costs orientation, not correctness, and must
   * not leave a Space unable to open.
   */
  private designateSelfConcept(): void {
    const found = super.executeKip(
      'FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: :key} } LIMIT 1',
      { key: SELF_ACTOR_KEY },
    )
    if (found.status === 'failed') return
    const rows = found.result
    const id = Array.isArray(rows) && typeof rows[0] === 'string' ? rows[0] : undefined
    if (id === undefined) return
    const concept = tryParseElementId(id)
    if (concept === null) return
    try {
      this.nexus.systemSession().designateSelf(concept)
    } catch {
      // Orientation, not authority: nothing this brain writes depends on it.
    }
  }
}
