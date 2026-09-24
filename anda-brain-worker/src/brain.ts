import { CurrentMemorySession, executeAgentOperations } from './agent-session.js'
import { MaintenanceWork, REVISED_ROOTS_KEY } from './maintenance.js'
import { forget, validateForget, type ForgetInput } from './forget.js'
import { MemoryProduct, assertCurrentOperations, type SourceIdentity, type ChangeInput, type RecordSource } from './product.js'
import { BRAIN_CAPABILITIES, runtimeOperations, type RuntimeOperation } from './cognitive.js'
import {
  KipDatabase,
  type AuthContext,
  KipError,
  isJsonMap,
  tryParseElementId,
  type JsonMap,
  type KipResult,
  type SchemaPackage,
} from '@ldclabs/kip-do'
import { DRAFT_PACKAGE_ID, DRAFT_PACKAGE_REF } from '@ldclabs/kip-do/schema'
import {
  assertFormationOperations,
  assertMaintenanceOperations,
  assertReadonlyOperations,
  isDefine,
  type IngestContext,
  type KipExecution,
  type KipOperation,
} from './kip.js'
import { assess, settle } from './settle.js'
import { recallAttention, type AttentionRecall, type AttentionRecallInput } from './attention.js'
import { MemoryLedger, type Admission, type IntakeRecord, type PassTrace, type StageSourceInput } from './memory-ledger.js'
import { descriptor, type AttentionItem, type Briefing, type MemoryRequest, type Progress, type Scope } from './memory-wire.js'
import type {
  Message,
  BrainStats,
  DeclaredVocabulary,
  DraftSymbol,
  Env,
  MaintenanceAssessment,
  PromoteDraftInput,
  PromoteDraftOutput,
  RevisedRoot,
  SchemaDrafts,
  SettlementReport,
} from './types.js'
import {
  MAX_SYMBOLS,
  MemoryVocabulary,
  activeSet,
  activeVocabulary,
  defineOf,
  definedRef,
  draftSymbols,
  queueSchemaReview,
} from './vocabulary.js'

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
  private get maintenance(): MaintenanceWork { return new MaintenanceWork(this.ctx.storage) }

  beginMaintenance(epoch: number, expiresAt: number): string {
    this.checkProcessing(epoch)
    return this.maintenance.begin(epoch, expiresAt)
  }
  endMaintenance(id: string): void { this.maintenance.release(id) }
  maintenanceSnapshot(epoch: number, run: string): KipResult[] {
    this.checkProcessing(epoch)
    this.maintenance.check(run, epoch)
    return this.executeAgentRead(this.maintenance.snapshot(this.nexus.space), epoch)
  }
  acknowledgeCorrections(ids: string[], epoch: number): void {
    this.checkProcessing(epoch)
    this.maintenance.acknowledge(ids)
  }

  private get memory(): MemoryLedger {
    this.ensureInitialized()
    return new MemoryLedger(this.nexus, this.ctx.storage, this.product, this.nexus.session(this.authenticate(undefined)))
  }

  // ── Memory Interface (see memory-ledger.ts) ────────────────────────────
  memoryStage(namespace: string, space: string, input: StageSourceInput) {
    return this.memory.stage(namespace, space, input)
  }
  memorySource(namespace: string, sourceRef: string) {
    return this.memory.stagedSource(namespace, sourceRef)
  }
  memoryAdmit(namespace: string, space: string, request: MemoryRequest): Admission {
    return this.memory.admit(namespace, space, request)
  }
  memoryCaptureEvidence(receiptRef: string, messages: Message[], observedAt: string, purpose: 'feedback' | 'revise-report', about?: JsonMap): IntakeRecord {
    return this.memory.captureEvidence(receiptRef, messages, observedAt, purpose, about)
  }
  memoryFinishFormation(receiptRef: string, trace: PassTrace): IntakeRecord {
    return this.memory.finishFormation(receiptRef, trace)
  }
  memoryFailFormation(receiptRef: string, error: { code: string; message: string }): IntakeRecord {
    return this.memory.failFormation(receiptRef, error)
  }
  memoryForget(namespace: string, space: string, request: MemoryRequest, owner: boolean): IntakeRecord {
    return this.memory.forget(namespace, space, request, owner)
  }
  memoryBarrier(namespace: string, after: string[]): Progress[] {
    return this.memory.barrier(namespace, after)
  }
  memoryScopedAttention(scope: Scope | undefined, items: AttentionRecall['items']): AttentionItem[] {
    return this.memory.scopedAttention(scope, items)
  }
  memoryDeliver(namespace: string, scope: Scope | undefined, cited: string[], options: Parameters<MemoryLedger['deliver']>[3]): Briefing {
    return this.memory.deliver(namespace, scope, cited, options)
  }
  memoryExpand(namespace: string, target: string, evidence: boolean): Briefing {
    return this.memory.expand(namespace, target, evidence)
  }
  memoryReceipt(namespace: string, receiptRef: string): JsonMap {
    return this.memory.receiptView(namespace, receiptRef)
  }
  memoryPlan(namespace: string, planRef: string): JsonMap {
    return this.memory.plan(namespace, planRef)
  }

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
    if (epoch > 0) {
      assertCurrentOperations(operations)
      assertReadonlyOperations(operations)
      return executeAgentOperations(new CurrentMemorySession(this.nexus, this.authenticate(undefined)), operations, true)
    }
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
    // Draft vocabulary (Spec §20.16): a DEFINE commits on its own and must
    // exist before the commands that use it, so the plan's DEFINEs run first,
    // one by one. One that already resolves (`SchemaSymbolConflict`) has no
    // effect rather than stopping the plan; each new symbol queues a review.
    const defines = operations.filter((operation) => isDefine(operation.command))
    const writes = operations.filter((operation) => !isDefine(operation.command))
    const results: KipResult[] = []
    if (defines.length > 0) {
      const vocabulary = MemoryVocabulary.load(this.nexus)
      if (vocabulary.size + defines.length > MAX_SYMBOLS) {
        throw new Error(`this Space's vocabulary holds ${vocabulary.size} of its ${MAX_SYMBOLS} symbols; ` +
          'reuse an existing symbol instead of defining another')
      }
      for (const define of defines) {
        const result = super.executeKip(define.command, define.parameters ?? {})
        if (result.error?.code === 'SchemaSymbolConflict') {
          results.push({ ...result, status: 'no_effect' })
          continue
        }
        results.push(result)
        if (result.status === 'failed') return results
        const ref = definedRef(result)
        const drafted = defineOf(define.command)
        if (ref && drafted) queueSchemaReview(this.host, drafted.kind, ref, drafted.description)
      }
    }
    if (writes.length === 0) return results
    return [...results, ...(epoch > 0
      ? executeAgentOperations(new CurrentMemorySession(this.nexus, this.authenticate(undefined)), writes, false, ingest)
      : super.executeKipBatch(writes, undefined, undefined, { mode: 'sequence', onError: 'stop' }, ingest))]
  }

  executeMaintenancePlan(operations: readonly KipOperation[], runtime: readonly RuntimeOperation[] = [], epoch = 0, run?: string, reviewed: string[] = []): KipResult[] {
    this.checkProcessing(epoch)
    if (run) this.maintenance.check(run, epoch)
    this.maintenance.validateAcknowledgement(reviewed)
    if (epoch > 0) assertCurrentOperations(operations)
    const work = runtimeOperations(runtime)
    if (operations.length > 0) assertMaintenanceOperations(operations)
    this.ensureInitialized()
    const session = this.nexus.session(this.authenticate(undefined))
    const results: KipResult[] = []
    for (const [index, request] of work.entries()) {
      try {
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
    const writes = epoch > 0
      ? executeAgentOperations(new CurrentMemorySession(this.nexus, this.authenticate(undefined)), operations, false)
      : super.executeKipBatch(operations, undefined, undefined, { mode: 'sequence', onError: 'stop' })
    const complete = [...results, ...writes]
    if (run && !complete.some(result => result.status === 'failed')) this.maintenance.acknowledge(reviewed)
    return complete
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
  settleMemory(nowMs: number, run?: string, epoch = 0): SettlementReport {
    this.checkProcessing(epoch)
    if (run) this.maintenance.check(run, epoch)
    const work = this.maintenance
    const report = settle((operation) => this.run(operation), nowMs, {
      advanceWatch: (id, version, generation) =>
        this.nexus.session(this.authenticate(undefined)).advanceWatch(id, version, generation, 200),
      correctionCursor: work.cursor(),
      pendingCorrections: work.pending(),
    })
    work.save(report.corrections)
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

  /** The settlement port as a command runner, for host-written vocabulary work. */
  private readonly host = (command: string, parameters: JsonMap): KipResult =>
    this.run({ command, parameters })

  /**
   * Drafts the bare names a Formation plan asked for (Spec §20.16).
   *
   * Formation's own `DEFINE` carries a real description; these names get the
   * host's generic one. The host validates each name, caps the Space's
   * vocabulary and queues one review per new symbol. A refused name is
   * reported, not raised: the plan's other commands are still writable.
   * Maintenance reviews drafts and never calls this.
   */
  declareSymbols(types: readonly string[], predicates: readonly string[], epoch = 0): DeclaredVocabulary {
    this.checkProcessing(epoch)
    this.ensureInitialized()
    const { defined, rejected } = draftSymbols(this.host, MemoryVocabulary.load(this.nexus), types, predicates)
    return MemoryVocabulary.load(this.nexus).declared(defined, rejected)
  }

  /** Every symbol this Space drafted, and the lineage each was promoted to. */
  schemaDrafts(): SchemaDrafts {
    this.ensureInitialized()
    const env = this.nexus.environment()
    const draft = env.lock.draft
    const maps = env.lock.lineage_maps ?? []
    const symbols: DraftSymbol[] = []
    for (const [section, kind] of [['concept_types', 'ConceptType'], ['predicates', 'PredicateType']] as const) {
      for (const [name, definition] of Object.entries(draft?.[section] ?? {}).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
        const promoted = maps.find((map) => map.kind === kind && map.from === `${DRAFT_PACKAGE_ID}/${name}`)
        symbols.push({ kind, name, ref: `${DRAFT_PACKAGE_REF}/${name}`, definition,
          ...(promoted ? { promoted_to: promoted.to } : {}) })
      }
    }
    return { package_ref: DRAFT_PACKAGE_REF, schema_environment_version: env.version, symbols }
  }

  /**
   * Promotes one draft symbol onto an installed symbol of the same kind: the
   * owner's Schema migration under `manage_schema`, never implicit, at most
   * once per symbol (Spec §20.16).
   */
  promoteDraftSymbol(input: PromoteDraftInput): PromoteDraftOutput {
    this.ensureInitialized()
    if (input.kind !== 'ConceptType' && input.kind !== 'PredicateType') {
      throw new KipError('ConstraintViolation', 'kind must be ConceptType or PredicateType')
    }
    if (typeof input.from !== 'string' || !input.from.trim() || typeof input.to !== 'string' || !input.to.trim()) {
      throw new KipError('ConstraintViolation', 'from and to are required')
    }
    const version = this.nexus.session(this.authenticate(undefined))
      .promoteDraftSymbol(input.kind, input.from, input.to)
    const name = input.from.startsWith(`${DRAFT_PACKAGE_REF}/`) ? input.from.slice(DRAFT_PACKAGE_REF.length + 1) : input.from
    return { promoted: `${DRAFT_PACKAGE_REF}/${name}`, to: input.to, schema_environment_version: version }
  }

  /** Attention recall (Memory Interface §4): read-only, ordered by `raised_seq`. */
  recallAttention(input: AttentionRecallInput): AttentionRecall {
    this.ensureInitialized()
    return recallAttention((command, parameters) =>
      super.executeKipBatch([{ command, parameters }], undefined, undefined, undefined, undefined, true)[0]!, input)
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
  private hostDeclared = false

  private ensureInitialized(): void {
    // The binding this Worker serves, declared to the engine so `DESCRIBE
    // CAPABILITIES`/`PRIMER` and every `requires` block report it truthfully
    // (Spec §67.4, MI §2). Process state: declared again after eviction.
    if (!this.hostDeclared) {
      this.nexus.setHostCapabilities({ memory_interface: descriptor(this.ctx.id.name) as never })
      this.hostDeclared = true
    }
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
