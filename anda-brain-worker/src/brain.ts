import { forget, validateForget, type ForgetInput } from './forget.js'
import { MemoryProduct, assertCurrentOperations, type SourceIdentity, type ChangeInput, type RecordSource } from './product.js'
import { BRAIN_CAPABILITIES, legacyRuntimeReplacement, runtimeOperations, type RuntimeOperation } from './cognitive.js'
import {
  KipDatabase,
  type AuthContext,
  KipError,
  isJsonMap,
  tryParseElementId,
  type KipResult,
  type SchemaPackage,
} from '@ldclabs/kip-do'
import {
  assertFormationOperations,
  assertMaintenanceOperations,
  assertReadonlyOperations,
  type IngestContext,
  type KipExecution,
  type KipOperation,
} from './kip.js'
import { assess, settle } from './settle.js'
import type {
  BrainStats,
  CorrectionCursor,
  DeclaredVocabulary,
  Env,
  MaintenanceAssessment,
  RevisedRoot,
  SettlementReport,
} from './types.js'
import { MemoryVocabulary, activeSet, activeVocabulary } from './vocabulary.js'

const APP_BOOTSTRAP_KEY = '__anda_brain_worker_bootstrap_version'
/** Where the next correction scan reads after. */
const CORRECTION_CURSOR_KEY = 'anda-brain:correction_cursor'
/** What the last settlement's correction scan found, for the next assessment. */
const REVISED_ROOTS_KEY = 'anda-brain:revised_roots'
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
  private get product(): MemoryProduct {
    return new MemoryProduct(this.nexus, this.ctx.storage, this.env.BRAIN_PRODUCT_RECIPIENT)
  }

  forgetMemory(input: ForgetInput) {
    validateForget(input)
    this.ensureInitialized()
    if (!input.dry_run) this.product.invalidate()
    return forget(this.nexus.session(this.authenticate(undefined)), input, ids => this.product.scrubChanges(ids))
  }
  beginProcessing(source?: SourceIdentity, origin?: string): number {
    this.ensureInitialized()
    return this.product.begin(source, origin)
  }
  checkProcessing(epoch: number): void {
    this.ensureInitialized()
    this.product.check(epoch)
  }
  executeAgentRead(operations: readonly KipOperation[], epoch: number): KipResult[] {
    this.checkProcessing(epoch)
    if (epoch > 0) assertCurrentOperations(operations)
    return this.executeKipReadonlyBatch(operations)
  }
  // Trusted RPC only. The embedding host supplies verified native authentication;
  // the public HTTP router never accepts AuthContext or exposes these methods.
  productRecords(auth: AuthContext, before?: number, limit?: number) {
    this.ensureInitialized()
    return this.product.records(auth, before, limit)
  }
  productRecord(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.record(auth, id)
  }
  productSource(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.source(auth, id)
  }
  productCorrectionSource(auth: AuthContext, source: RecordSource) {
    this.ensureInitialized()
    return this.product.correctionSource(auth, source)
  }
  productPrepare(auth: AuthContext, input: ChangeInput) {
    this.ensureInitialized()
    return this.product.prepare(auth, input)
  }
  productChange(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.change(auth, id)
  }
  productCommit(auth: AuthContext, id: string, previewDigest: string) {
    this.ensureInitialized()
    return this.product.commit(auth, id, previewDigest)
  }
  productDiscard(auth: AuthContext, id: string) {
    this.ensureInitialized()
    this.product.discard(auth, id)
  }
  productStatus() {
    this.ensureInitialized()
    const state = this.product.state()
    return {
      epoch: state.epoch, available: !state.pending, pending: state.pending,
      learning_readiness: {
        state: 'services_missing', next_step: 'use_rust_learning_runtime_with_explicit_bindings', supported: false,
      },
    }
  }
  productRecordWatch(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.watch(auth, id)
  }
  productCreateRecordWatch(auth: AuthContext, id: string, target: string, summary: string) {
    this.ensureInitialized()
    return this.product.createWatch(auth, id, target, summary)
  }
  productAdvanceRecordWatch(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.advanceWatch(auth, id)
  }
  productCancelRecordWatch(auth: AuthContext, id: string) {
    this.ensureInitialized()
    return this.product.cancelWatch(auth, id)
  }

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

  /**
   * Every argument is forwarded. All of them, every time.
   *
   * These two overrides exist to call `ensureInitialized` and for nothing else,
   * and an override written for one reason drops the parameters it did not
   * think about — silently, because the base class simply sees `undefined` and
   * TypeScript accepts a narrower override. Each of the ones this signature has
   * had to grow fails quietly on its own terms:
   *
   * - `ingest` is where the observation's real bytes are (§71.1), so losing it
   *   turns `:msg1` into an unbound parameter, and the model is told its
   *   command is malformed for a facility the runtime failed to deliver;
   * - `idempotencyKey` is §26's "a timeout is not an abort" — drop it and a
   *   resend of a lost write is a second write instead of the first one's
   *   receipt;
   * - `execution` defaulted would run a batch the caller asked to stop at the
   *   first failure as `independent`, committing writes it asked to have
   *   skipped, while the answer still said `sequence` because the envelope
   *   reports what was requested;
   * - `readonly` is the engine's own mutation refusal, which arrived after
   *   these overrides were written and which a hand-listed signature therefore
   *   dropped without a word.
   *
   * So the arguments are forwarded as a tuple rather than named. Restating the
   * base signature is what keeps going stale; this cannot, because there is
   * nothing here to restate.
   */
  override executeKip(
    ...args: Parameters<KipDatabase<Env>['executeKip']>
  ): KipResult {
    this.ensureInitialized()
    return super.executeKip(...args)
  }

  override executeKipBatch(
    ...args: Parameters<KipDatabase<Env>['executeKipBatch']>
  ): KipResult[] {
    this.ensureInitialized()
    return super.executeKipBatch(...args)
  }

  /**
   * KQL and META only, decided by what each command parses to — twice.
   *
   * {@link assertReadonlyOperations} is the gate that answers: it refuses the
   * whole batch before anything runs, naming the offending command, which is
   * the error a caller can act on. The engine's own `readonly` flag is the
   * floor underneath it, and it earns its place by being independent — it is
   * checked inside the statement path, against `parseKip`'s own verdict, after
   * every envelope field has been read. So a hole in the gate above costs one
   * failed operation rather than a committed mutation.
   *
   * Both decide on parsed semantics, never on a label the caller attached. A
   * request that calls itself a query and carries a mutation is refused by the
   * gate, and would be refused by the engine if it were not.
   */
  executeKipReadonlyBatch(
    operations: readonly KipOperation[],
    execution?: KipExecution,
  ): KipResult[] {
    assertReadonlyOperations(operations)
    this.ensureInitialized()
    return super.executeKipBatch(operations, undefined, undefined, execution, undefined, true)
  }

  /**
   * Runs a formation plan, with the observation it was formed from.
   *
   * `ingest` is the envelope's §71.1 block: the runtime's own copy of what was
   * said, minted as Evidence inside each statement's transaction so the bytes
   * never pass through model-generated text. It rides every operation in the
   * batch and dedupes on `client_key`, so four commands citing `:msg1` cite one
   * Evidence record rather than minting four.
   */
  executeFormationPlan(
    operations: readonly KipOperation[],
    ingest?: IngestContext,
    epoch = 0,
  ): KipResult[] {
    this.checkProcessing(epoch)
    if (epoch > 0) assertCurrentOperations(operations)
    assertFormationOperations(operations)
    this.ensureInitialized()
    this.product.capture(ingest)
    return super.executeKipBatch(operations, undefined, undefined, { mode: 'sequence', onError: 'stop' }, ingest)
  }

  executeMaintenancePlan(operations: readonly KipOperation[], runtime: readonly RuntimeOperation[] = [], epoch = 0): KipResult[] {
    this.checkProcessing(epoch)
    if (epoch > 0) assertCurrentOperations(operations)
    const work = runtimeOperations(runtime)
    if (operations.length > 0) assertMaintenanceOperations(operations)
    this.ensureInitialized()
    const session = this.nexus.session(this.authenticate(undefined))
    const results: KipResult[] = []
    for (const [index, request] of work.entries()) {
      try {
        const id = tryParseElementId(request.target_ref)!
        const target = this.nexus.store.load(id)
        const schemaRef = target?.kind === 'Concept' ? target.row.schema_ref : ''
        const legacy = legacyRuntimeReplacement(request.operation, schemaRef)
        if (legacy) throw new KipError('UnsupportedCapability', legacy)
        const result = request.operation === 'arm_watch'
          ? session.armWatch(request.target_ref, request.expected_version)
          : session.leaseTask(request.target_ref, request.expected_version, new Date(Date.now() + 300_000).toISOString())
        results.push({
          op_id: `runtime_${index}`,
          status: isJsonMap(result.receipt) && result.receipt.status === 'no_effect' ? 'no_effect' : 'succeeded',
          result,
        })
      } catch (error) {
        results.push({ op_id: `runtime_${index}`, status: 'failed', error: KipError.from(error).toJSON() })
        return results
      }
    }
    return [...results, ...super.executeKipBatch(operations, undefined, undefined, { mode: 'sequence', onError: 'stop' })]
  }

  describePrimer(): KipResult {
    this.ensureInitialized()
    const result = super.executeKip('DESCRIBE PRIMER')
    if (result.result && typeof result.result === 'object' && !Array.isArray(result.result)) {
      return {
        ...result,
        result: {
          ...result.result,
          extensions: {
            ...(isJsonMap(result.result.extensions) ? result.result.extensions : {}),
            'anda-brain/capabilities': BRAIN_CAPABILITIES,
          },
        },
      }
    }
    return result
  }

  /**
   * The deterministic settlement, run before every maintenance cycle.
   *
   * Everything it does is arithmetic the host can do without a model, and the
   * reason it did not exist before is worth naming: the maintenance model gets
   * one completion and emits KML, so a pass needing a read before a write
   * looked impossible. It was only impossible for the model — this object holds
   * the nexus, and {@link run} hands the settlement the one capability it
   * cannot supply itself.
   */
  settleMemory(nowMs: number, decayFactor?: number): SettlementReport {
    this.ensureInitialized()
    const kv = this.ctx.storage.kv
    const report = settle((operation) => this.run(operation), nowMs, {
      decayFactor,
      advanceWatch: (id, version, generation) =>
        this.nexus.session(this.authenticate(undefined)).advanceWatch(id, version, generation, 200),
      correctionCursor: kv.get<number | CorrectionCursor>(CORRECTION_CURSOR_KEY),
    })
    // A scan that failed leaves the cursor where it was, so nothing it did
    // not read falls behind the watermark.
    if (report.corrections.error === undefined) {
      kv.put(CORRECTION_CURSOR_KEY, {
        seq: report.corrections.cursor,
        after_id: report.corrections.cursor_after_id ?? '',
      })
    }
    kv.put(REVISED_ROOTS_KEY, report.corrections.revised_roots)
    return report
  }

  /** What the settlement measured, as the maintenance prompt receives it. */
  maintenanceAssessment(): MaintenanceAssessment {
    this.ensureInitialized()
    const kv = this.ctx.storage.kv
    return assess(
      (operation) => this.run(operation),
      this.nexus.store.currentSeq(this.nexus.space),
      {
        revisedRoots: kv.get<RevisedRoot[]>(REVISED_ROOTS_KEY) ?? [],
      },
    )
  }

  /**
   * The settlement's port: one command in, one result out.
   *
   * `super`, not `this`: the guard has already run by the time a settlement
   * starts, and re-entering the override would cost a storage read per command
   * for a check that cannot fail twice.
   */
  private run(operation: KipOperation): KipResult {
    return super.executeKip(operation.command, operation.parameters)
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
  declareSymbols(types: readonly string[], predicates: readonly string[], epoch = 0): DeclaredVocabulary {
    this.checkProcessing(epoch)
    this.ensureInitialized()
    const vocabulary = MemoryVocabulary.load(this.nexus)
    const before = vocabulary.revision
    const rejected = vocabulary.extend(types, predicates)
    if (vocabulary.revision !== before) vocabulary.activate(this.nexus)
    return vocabulary.declared(rejected)
  }

  /** What this Space can already say, without publishing anything. */
  vocabulary(): DeclaredVocabulary {
    this.ensureInitialized()
    return MemoryVocabulary.load(this.nexus).declared()
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
      this.product.recover()
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
