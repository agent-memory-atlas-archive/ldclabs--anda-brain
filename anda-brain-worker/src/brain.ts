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
import * as settle from './settle.js'
import type {
  BrainStats,
  DeclaredVocabulary,
  Env,
  MaintenanceAssessment,
  SettlementReport,
} from './types.js'
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
   * The deterministic settlement, run before every maintenance cycle.
   *
   * Everything here is arithmetic the host can do without a model, and the
   * reason it did not exist before is worth naming: the maintenance model gets
   * one completion and emits KML, so a pass needing a read before a write
   * looked impossible. It was only impossible for the model — this object holds
   * the nexus, and reads it directly.
   *
   * Synchronous throughout, like everything else under it, and never allowed to
   * fail the cycle: a settlement that could not sweep is a degraded cycle, not
   * a failed one, and each pass reports its own error so "nothing was due" and
   * "the pass never ran" stay distinguishable.
   */
  settleMemory(nowMs: number): SettlementReport {
    this.ensureInitialized()
    const now = new Date(nowMs).toISOString()
    const report = settle.emptySettlement(now)
    report.decayed = this.metabolize(now, new Date(settle.decayHorizon(nowMs)).toISOString())
    report.watches = this.fireDueWatches(now)
    report.skills = this.settleSkillLifecycle(now)
    return report
  }

  /**
   * Disuse metabolism: `MnemonicState.memory_strength`, never confidence.
   *
   * The `last_metabolized_at` filter is both the weekly rate limit and the
   * intra-sweep cursor, so a Space with nothing due costs one query that
   * matches no rows.
   */
  private metabolize(now: string, metabolizedBefore: string): number {
    const operation = settle.decayCommand(now, metabolizedBefore)
    const result = super.executeKip(operation.command, operation.parameters)
    if (result.status === 'failed') return 0
    const outcome = result.extensions?.['kip-do/outcome']
    return (outcome?.changes ?? []).filter((change) => change.op === 'update').length
  }

  /** Fires the silence Watches whose deadline has passed. */
  private fireDueWatches(now: string): SettlementReport['watches'] {
    const report = settle.emptyWatchSettlement()
    const scan = settle.dueSilenceWatchesCommand(now)
    const found = super.executeKip(scan.command, scan.parameters)
    if (found.status === 'failed') {
      report.error = found.error?.message ?? 'watch scan failed'
      return report
    }
    for (const due of settle.dueWatches(found.result)) {
      const fire = settle.fireWatchCommand(due, now)
      const written = super.executeKip(fire.command, fire.parameters)
      if (written.status === 'failed') report.conflicted += 1
      else report.fired += 1
    }
    return report
  }

  /**
   * Runs the deterministic Skill lifecycle rule over graded outcomes.
   *
   * Profile §14 rule 1: promotion and demotion are executed by deterministic
   * code reading graded Outcome Evidence — "the Brain proposes, compiles, and
   * narrates; it never promotes."
   */
  private settleSkillLifecycle(now: string): SettlementReport['skills'] {
    const report = settle.emptySkillSettlement()
    const scan = settle.skillsCommand()
    const found = super.executeKip(scan.command, scan.parameters)
    if (found.status === 'failed') {
      report.error = found.error?.message ?? 'skill scan failed'
      return report
    }
    for (const skill of settle.skillRows(found.result)) {
      // One read per Skill: the window is per-family and per-cursor, and a
      // Skill never graded starts from a different coordinate than one that has.
      const outcomes = settle.outcomesCommand(skill.task_family, skill.cursor)
      const graded = super.executeKip(outcomes.command, outcomes.parameters)
      if (graded.status === 'failed') continue
      const window = settle.tally(graded.result)
      const verdict = settle.decide(skill, window)
      // No new graded outcome: an idle stream writes nothing.
      if (verdict === undefined) continue

      const write = settle.verdictCommand(skill, verdict, window, now)
      const written = super.executeKip(write.command, write.parameters)
      if (written.status === 'failed') {
        // The cursor did not advance, so the next pass re-reads the same
        // outcomes and reaches the same verdict.
        report.conflicted += 1
        continue
      }
      report.graded += 1
      if (verdict.transition !== undefined) report.transitions += 1
    }
    return report
  }

  /**
   * What the settlement measured, as the maintenance prompt receives it.
   *
   * The model cannot go and fetch any of this — it gets one completion — so a
   * signal absent here is a duty it will not perform. That is why the armed
   * set, the fired queue and `space_seq` are read for it rather than left to
   * a §6 assessment it has no way to run.
   */
  maintenanceAssessment(): MaintenanceAssessment {
    this.ensureInitialized()
    const watches = (status: string) => {
      const operation = settle.watchesCommand(status)
      const result = super.executeKip(operation.command, operation.parameters)
      return result.status === 'failed' ? [] : settle.readWatches(result.result)
    }
    return {
      space_seq: this.nexus.store.currentSeq(this.nexus.space),
      armed_watches: watches('armed'),
      fired_watches: watches('fired'),
      predicates: this.predicateCensus(),
    }
  }

  /**
   * Per-predicate link counts — the vocabulary sprawl indicator.
   *
   * A count that failed is omitted rather than reported as zero: naming the
   * busiest predicate as unused would point the merge guidance at exactly the
   * wrong target.
   */
  private predicateCensus(): Record<string, number> {
    const census: Record<string, number> = {}
    const listed = super.executeKip('LIST PREDICATES LIMIT 100')
    if (listed.status === 'failed' || !Array.isArray(listed.result)) return census
    for (const entry of listed.result) {
      // A `LIST` row, the same one both engines answer with:
      // `{ref, local_name, package_ref, status}`. `local_name` is what a
      // command may write, which is what the census counts by.
      if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) continue
      const name = (entry as { local_name?: unknown }).local_name
      if (typeof name !== 'string' || name === '') continue
      const operation = settle.predicateCensusCommand(name)
      const counted = super.executeKip(operation.command, operation.parameters)
      if (counted.status === 'failed') continue
      const rows = counted.result
      const count = Array.isArray(rows) ? rows[0] : undefined
      if (typeof count === 'number') census[name] = count
    }
    return census
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
