/**
 * The KIP 2.0 seam: operations in, results out, and the gates in between.
 *
 * KIP 2.0 replaced the 1.x `command: string` pair with an operation — a command
 * plus its own parameter bindings — and replaced the single response with one
 * {@link KipResult} per operation. Parameters ride the envelope rather than the
 * command text, which is what keeps a value that came from a model or a user
 * from being read as syntax.
 *
 * Every gate here decides on what a command **parses to**, never on a label the
 * caller attached to it. A request that calls itself a query and carries a
 * mutation is the mutation it is, or it is nothing.
 */

import {
  parseKip,
  tryParseElementId,
  type Command,
  type Json,
  type JsonMap,
  type KipResult,
  type MetaCommand,
  type IngestContext,
  type MutationClause,
  type Outcome,
  type Scalar,
} from '@ldclabs/kip-do'
import type { MemoryCitation, Message } from './types.js'

export const MAX_KIP_OPERATIONS = 4
const MAX_KIP_COMMAND_BYTES = 256 * 1024

/** The most rows a model-planned read may ask for. */
const MAX_MODEL_RESULTS = 20

/** The most elements one maintenance clause may select. */
const MAX_MAINTENANCE_SELECTION = 20

/** The most citations one answer carries. */
const MAX_CITATIONS = 16

/**
 * The most messages one formation pass mints as Evidence.
 *
 * The newest ones, because a formation pass writes about what was just said and
 * an envelope carrying an unbounded transcript is a request nobody bounded. The
 * older turns are still in the prompt; what a message past this line loses is
 * the *verbatim* record, so a claim resting on one has to be written the long
 * way — which the contract says, so the model is not left guessing why `:msg17`
 * does not resolve.
 */
const MAX_INGESTED_MESSAGES = 16

/**
 * What a message's role makes it, as an Evidence class (Formation §7).
 *
 * A transcript is not one observation. Who said a thing is part of what was
 * observed, and flattening four turns into one `payload` would leave a later
 * reader unable to tell the user's words from the assistant's — which is the
 * distinction an attributed claim rests on.
 */
const EVIDENCE_CLASS: Record<string, string> = {
  user: 'user_statement',
  assistant: 'agent_statement',
  tool: 'tool_result',
  system: 'message',
}

/** The `ingest` block of a request envelope. */
export type { IngestContext }

/**
 * The observation this formation pass was called on, ready for the runtime to
 * mint (Spec §71.1, Formation §7).
 *
 * The point is fidelity, and it is worth stating plainly: a model that retypes
 * an observation into `CREATE EVIDENCE ... {payload: "…"}` truncates it,
 * normalizes its whitespace, fixes its spelling, or paraphrases it — and the
 * record then says the source said something it did not (§88.12). So the
 * payload the runtime received is the payload that is stored, and the model
 * only ever writes `:msg1`.
 *
 * `client_key` is what makes this safe to attach to every operation in the
 * batch and to a resend: the first mint wins and the rest resolve to it. Its
 * stability is only as good as `origin` — see `conversationOrigin`.
 *
 * `source_actor` has to name something a reader can follow — an element
 * reference, `{id}` or `{type, key}` — and `context.counterparty` is a Concept
 * *key* without its type, so it is
 * the caller's job to resolve one. Formation ensures the counterparty before
 * planning, so its first ingestion and its retries use the same source. A
 * source remains semantic provenance; attribution still lives in `asserted_by`
 * on the Assertion.
 */
/**
 * When one captured message was observed: its own millisecond timestamp when
 * the caller sent one, else the batch's observation time. This is what an
 * Assertion citing the message writes as `at` (Spec §13.2).
 */
export function messageObservedAt(message: Message, batchAt: string): string {
  if (typeof message.timestamp !== 'number') return batchAt
  const date = new Date(message.timestamp)
  const text = Number.isFinite(date.getTime()) ? date.toISOString() : ''
  return text.length === 24 ? text : batchAt
}

/** The captured window's Evidence keys and observation times, for the model's `at`. */
export function capturedEvidence(
  messages: readonly Message[],
  batchAt: string,
): { key: string; evidence_class: string; observed_at: string }[] {
  const start = Math.max(0, messages.length - MAX_INGESTED_MESSAGES)
  return messages.slice(start).map((message, index) => ({
    key: `msg${index + 1}`,
    evidence_class: EVIDENCE_CLASS[message.role] ?? 'message',
    observed_at: messageObservedAt(message, batchAt),
  }))
}

export function observationIngest(
  operations: readonly KipOperation[],
  messages: readonly Message[],
  observed: { at: string; origin: string; sourceActor?: string },
): IngestContext | undefined {
  const start = Math.max(0, messages.length - MAX_INGESTED_MESSAGES)
  const recent = messages.slice(start)
  // A pass that stored nothing has nothing to mint Evidence for, and an
  // Evidence record for a claim nobody made is indistinguishable later from an
  // observation somebody chose not to act on.
  if (operations.length === 0 || recent.length === 0) return undefined
  // Numbered from the start of the kept window, so `:msg1` is the oldest
  // message the model can cite and the numbering matches the order it reads
  // them in.
  const evidence = recent.map((message, index) => ({
    key: `msg${index + 1}`,
    evidence_class: EVIDENCE_CLASS[message.role] ?? 'message',
    payload: message as unknown as Json,
    observed_at: messageObservedAt(message, observed.at),
    client_key: `${observed.origin}:${start + index + 1}`,
    ...(observed.sourceActor === undefined || message.role !== 'user'
      ? {}
      : { source_actor: { id: observed.sourceActor } }),
  }))

  // §74 merges request- and operation-level parameters into one binding
  // environment, so a name this block would mint that the plan already binds
  // makes `:msg1` ambiguous and the engine refuses the whole request. A model
  // that bound the name itself is writing Evidence the long way; let it, rather
  // than failing its plan over a facility it did not ask for.
  const keys = new Set(evidence.map((entry) => entry.key))
  const claimed = operations.some((operation) =>
    Object.keys(operation.parameters ?? {}).some((name) => keys.has(name)),
  )
  return claimed ? undefined : { evidence }
}

/** One command plus the values bound into its `:placeholders`. */
export interface KipOperation {
  /**
   * The request-local name the caller pairs this operation's answer with (§73).
   *
   * Optional and carried through untouched. The engine echoes it on the result,
   * which is the only way to read a batch whose answers are not all present —
   * a `sequence` that stopped answers `skipped` for the rest, and matching by
   * position works right up until a caller reorders its own operations.
   */
  op_id?: string
  command: string
  parameters?: JsonMap
}

/**
 * How a batch runs, in the shape the engine's `executeKipBatch` takes.
 *
 * Spelled out here because `@ldclabs/kip-do` keeps the union types private;
 * the values are §75's and are checked against the engine's own list on the
 * envelope path, so a wrong one is refused rather than defaulted.
 */
export interface KipExecution {
  mode: 'independent' | 'sequence' | 'atomic'
  onError: 'stop' | 'continue'
}

/**
 * The clauses Formation may write.
 *
 * Formation encodes what was observed: Concepts, the Propositions relating
 * them, the Evidence they rest on and the Assertions that take a stance. It
 * also corrects — which in KIP 2.0 is a *new* Assertion plus supersession, so
 * `TRANSITION` belongs here even though it changes a claim's standing.
 *
 * What is missing is deliberate. `UPDATE`, `PURGE` and `MERGE` act on memory in
 * bulk from a selection, and a formation pass reading an untrusted conversation
 * is the last thing that should hold them.
 */
const FORMATION_CLAUSES = new Set([
  'CreateConcept',
  'UpsertConcept',
  'EnsureProposition',
  'CreateEvidence',
  'CreateAssertion',
  'CreateActivity',
  'Transition',
])

/**
 * The lifecycle states Formation may name (§52.5).
 *
 * KIP 2.0 collapsed six lifecycle statements into one `TRANSITION`, so the
 * split that used to fall between verbs now falls inside one: correcting
 * cognition stays, removing memory does not. Everything §52.5 registers except
 * `archived` and `tombstoned`.
 */
const FORMATION_TRANSITIONS = new Set([
  'retracted',
  'superseded',
  'corrected',
  'running',
  'completed',
  'failed',
  'cancelled',
])

function assertOperationBatch(operations: readonly KipOperation[]): void {
  if (operations.length === 0) {
    throw new Error('a KIP request needs at least one operation')
  }
  if (operations.length > MAX_KIP_OPERATIONS) {
    throw new Error(`at most ${MAX_KIP_OPERATIONS} KIP operations are allowed`)
  }

  let bytes = 0
  for (const operation of operations) {
    if (typeof operation?.command !== 'string' || operation.command.trim() === '') {
      throw new Error('every KIP operation needs a non-empty command')
    }
    bytes += new TextEncoder().encode(operation.command).byteLength
  }
  if (bytes > MAX_KIP_COMMAND_BYTES) {
    throw new Error(`the KIP batch exceeds ${MAX_KIP_COMMAND_BYTES} bytes`)
  }
}

/** Rejects anything that parses to KML, whatever the caller called it. */
export function assertReadonlyOperations(operations: readonly KipOperation[]): void {
  assertOperationBatch(operations)
  for (const operation of operations) {
    if ('Kml' in parseKip(operation.command)) {
      throw new Error('read-only KIP accepts only KQL and META commands')
    }
  }
}

/**
 * The read-only gate a *model's* plan passes, which is narrower.
 *
 * An unbounded read is not merely expensive here: the answer is assembled from
 * whatever came back, so a `FIND` with no `LIMIT` decides how much of the graph
 * ends up in a prompt.
 */
function assertBoundedReadonlyOperations(
  operations: readonly KipOperation[],
  maxResults: number,
): void {
  assertOperationBatch(operations)
  for (const operation of operations) {
    assertBoundedRead(parseKip(operation.command), operation.parameters, maxResults)
  }
}

/** Formation writes cognition; it does not administer memory. */
export function assertFormationOperations(operations: readonly KipOperation[]): void {
  assertOperationBatch(operations)
  for (const operation of operations) {
    const command = parseKip(operation.command)
    if (!('Kml' in command)) {
      throw new Error('formation accepts only KIP KML commands')
    }
    for (const clause of command.Kml.clauses) {
      assertCognitiveRecords(clause, operation.parameters)
      const name = clauseName(clause)
      if (!FORMATION_CLAUSES.has(name)) {
        throw new Error(
          `formation cannot issue ${spelling(name)}; it writes cognition ` +
            '(CREATE / UPSERT / ENSURE / ASSERT) and corrects it ' +
            '(TRANSITION to retracted / superseded / corrected)',
        )
      }
      if ('Transition' in clause) {
        const state = transitionState(clause.Transition.to, operation.parameters)
        if (state === undefined) {
          throw new Error(
            'formation must write the TRANSITION state as a literal this request ' +
              'can be read against; a state bound elsewhere cannot be checked ' +
              'before it runs',
          )
        }
        if (!FORMATION_TRANSITIONS.has(state)) {
          throw new Error(
            `formation cannot TRANSITION memory to "${state}"; it corrects ` +
              'cognition (retracted / superseded / corrected) and runs its own ' +
              'Activities. Removing memory belongs to maintenance',
          )
        }
      }
      assertBoundedSelection(clause, operation.parameters, MAX_MAINTENANCE_SELECTION)
    }
  }
}

/**
 * Maintenance may consolidate and retire, but never erase.
 *
 * `PURGE` is irreversible, and a model reading its own graph snapshot is not
 * the right place to decide that something should stop having existed. Raw
 * `execute_kip` is, which is why that endpoint is administrative.
 *
 * `PURGE PAYLOAD` (§60.6) is refused on the same grounds and not lesser ones.
 * It leaves the Evidence record, its digest and its citations standing and
 * destroys the bytes underneath them — so an Assertion keeps pointing at an
 * observation whose content is gone. That is a narrower blast radius than
 * element purge, not a reversible one, and the reference policy reaches for it
 * *after* digestion, which is a judgement about what was already extracted
 * rather than one a single planning pass can make from a snapshot.
 */
export function assertMaintenanceOperations(operations: readonly KipOperation[]): void {
  assertOperationBatch(operations)
  for (const operation of operations) {
    const command = parseKip(operation.command)
    if (!('Kml' in command)) {
      throw new Error('maintenance plans must contain KML commands')
    }
    for (const clause of command.Kml.clauses) {
      assertCognitiveRecords(clause, operation.parameters)
      if ('Purge' in clause || 'PurgePayload' in clause) {
        throw new Error('maintenance plans cannot issue KIP PURGE commands')
      }
      assertNoLegalHold(clause)
      assertBoundedSelection(clause, operation.parameters, MAX_MAINTENANCE_SELECTION)
    }
  }
}

/**
 * Maintenance may set retention, but never place or lift a legal hold.
 *
 * `SET RETENTION` itself is exactly what §20 Retention Review and §25 Retention
 * Expiry are for: a retention class and an `expires_at` are storage policy, and
 * the removal they schedule is a host-run sweep that a Principal is accountable
 * for. A model may make that judgement.
 *
 * `legal_hold` is the member it may not touch, in either direction. A hold
 * blocks erasure for everyone (§60.6), so content that could set one could make
 * itself undeletable, and content that could clear one could unblock an erasure
 * somebody placed a hold to stop. Neither is a decision to reach from a graph
 * snapshot; both go through the administrative `execute_kip` endpoint, where a
 * human is the one asking.
 *
 * Checked on the member name, which the grammar fixes, so a parameterized value
 * cannot smuggle it past: `{legal_hold: :whatever}` is refused on the name
 * alone, before anything is evaluated.
 */
function assertNoLegalHold(clause: MutationClause): void {
  if (!('SetRetention' in clause)) return
  for (const [name] of clause.SetRetention.values) {
    if (name === 'legal_hold') {
      throw new Error(
        'maintenance plans cannot place or lift a legal hold; set a retention ' +
          'class and an expires_at, and leave the hold to the administrative ' +
          'endpoint',
      )
    }
  }
}

/**
 * Keeps the model-planned reads that are safe and drops the rest.
 *
 * A discarded command is not an error: the deterministic lookup still gives
 * recall its evidence, and failing the whole request because a planner emitted
 * one unbounded `FIND` would turn a recoverable plan into no answer at all.
 */
export function keepReadonlyOperations(
  operations: readonly KipOperation[],
): KipOperation[] {
  const valid: KipOperation[] = []
  for (const operation of operations) {
    try {
      assertBoundedReadonlyOperations([operation], MAX_MODEL_RESULTS)
      valid.push(operation)
    } catch {
      // Invalid, mutating or unbounded: dropped rather than repaired.
    }
  }
  return valid
}

/**
 * The deterministic grounding read.
 *
 * Associative grounding is what `SEARCH` is for (§66.1), and it runs before the
 * model is asked anything: the answer should not depend on a planner having
 * thought to look. `LIMIT` and the term ride the envelope as parameters rather
 * than being spliced into the command, so a query that contains a quote is a
 * query and not syntax.
 *
 * A miss here is a miss on the index, not an absence — which is why recall says
 * so in its answer rather than reporting "no".
 */
export function conceptLookupCommand(query: string, limit = 8): KipOperation {
  const bounded = Math.max(1, Math.min(MAX_MODEL_RESULTS, Math.trunc(limit)))
  return {
    command: 'SEARCH CONCEPT :term LIMIT :limit',
    parameters: { term: query, limit: bounded },
  }
}

/**
 * What a mutation actually committed.
 *
 * §81 closes `OperationResult` to the fields the normative schema names, so the
 * engine's full transaction outcome — handles, per-element changes, the
 * governance decision — moved under a namespaced `extensions` slot. Read
 * through one accessor rather than at each site: an operation that failed, was
 * skipped or was a read has none, and the difference between "no outcome" and
 * "an outcome that changed nothing" is the caller's to keep.
 */
function outcomeOf(result: KipResult): Outcome | undefined {
  return result.extensions?.['kip-do/outcome']
}

/**
 * The lifecycle states that retire an element rather than advance it.
 *
 * `TRANSITION` reports one op — `lifecycle` — for nine states, so the op alone
 * no longer says whether a memory was withdrawn or an Activity simply started
 * running. §36.1 puts the move in `state.to`, and this is the half of it the
 * API's `retired` count has always meant.
 */
const RETIRING_STATES = new Set([
  'retracted',
  'superseded',
  'corrected',
  'archived',
  'tombstoned',
])

/** What one write produced, in the shape the API has always reported. */
export function countChanges(results: readonly KipResult[]): {
  total: number
  created: number
  updated: number
  retired: number
  merged: number
} {
  let created = 0
  let updated = 0
  let retired = 0
  let merged = 0
  for (const result of results) {
    const changes = outcomeOf(result)?.changes
    // Brain runtime operations are protected Session calls rather than KML,
    // so their transaction outcome is nested under `result` instead of the
    // kip-do extension. Each committed arm/lease changes exactly one Concept.
    if (changes === undefined && result.op_id?.startsWith('runtime_') && result.status === 'succeeded') {
      updated += 1
    }
    for (const change of changes ?? []) {
      switch (change.op) {
        case 'create':
          created += 1
          break
        case 'update':
        case 'retention':
          updated += 1
          break
        case 'lifecycle':
          if (RETIRING_STATES.has(change.state?.to ?? '')) retired += 1
          else updated += 1
          break
        case 'merge':
          merged += 1
          break
        case 'purge':
        case 'payload_purge':
          retired += 1
          break
      }
    }
  }
  return { total: created + updated + retired + merged, created, updated, retired, merged }
}

/** How many Concepts and Propositions a formation batch created. */
export function countWrites(results: readonly KipResult[]): {
  concepts: number
  propositions: number
  assertions: number
} {
  let concepts = 0
  let propositions = 0
  let assertions = 0
  for (const result of results) {
    for (const change of outcomeOf(result)?.changes ?? []) {
      if (change.op !== 'create') continue
      // A change record spells its Core kind the way `?c.kind` does — lowercase,
      // like every other wire tag.
      if (change.kind === 'concept') concepts += 1
      else if (change.kind === 'proposition') propositions += 1
      else if (change.kind === 'assertion') assertions += 1
    }
  }
  return { concepts, propositions, assertions }
}

/**
 * Citations from a grounding lookup.
 *
 * A `SEARCH` answer is `{hits: [{id, kind, score, element}], …}`, and the hit is
 * an **envelope**: reading `name` and `schema_ref` off the wrapper instead of
 * off `element` yields a citation with an id and nothing else, which looks like
 * a working citation list right up until somebody reads one.
 *
 * The score is deliberately not carried into the citation. It is retrieval
 * relevance, and a number sitting beside a memory is read as confidence (§2.10).
 */
function citationsFromLookup(result: Json | undefined): MemoryCitation[] {
  const hits = isObject(result) ? result.hits : undefined
  if (!Array.isArray(hits)) return []
  const citations: MemoryCitation[] = []
  for (const hit of hits) {
    if (!isObject(hit)) continue
    const id = hit.id
    if (typeof id !== 'string' || tryParseElementId(id) === null) continue
    const element = isObject(hit.element) ? hit.element : {}
    const citation: MemoryCitation = { entity: id }
    if (typeof element.name === 'string' && element.name !== '') {
      citation.name = element.name
    }
    if (typeof element.schema_ref === 'string' && element.schema_ref !== '') {
      citation.type = localName(element.schema_ref)
      citation.schema_ref = element.schema_ref
    }
    citations.push(citation)
  }
  return citations.slice(0, MAX_CITATIONS)
}

/**
 * What an answer rests on: the grounding hits, then whatever the planned reads
 * turned up.
 *
 * The two halves are read differently and both ways are deliberate. A `SEARCH`
 * answer is a shaped envelope, so its hits yield a name and a type. A planned
 * command projected whatever it liked, so nothing here knows which column holds
 * what and an id alone is all that can honestly be claimed — which is still a
 * usable citation, because it names an element a reader can go and look at.
 *
 * The grounding read comes first for the same reason it runs first: its
 * citations are the ones that carry more than an id, and the cap must not spend
 * itself on bare ids before reaching them.
 */
export function citationsFrom(
  grounding: KipResult | undefined,
  planned: readonly KipResult[] = [],
): MemoryCitation[] {
  const citations = new Map<string, MemoryCitation>()
  for (const citation of citationsFromLookup(grounding?.result)) {
    citations.set(citation.entity, citation)
  }
  for (const result of planned) visit(result.result, citations)
  return [...citations.values()].slice(0, MAX_CITATIONS)
}

function visit(value: unknown, citations: Map<string, MemoryCitation>): void {
  if (citations.size >= MAX_CITATIONS) return
  if (typeof value === 'string') {
    if (tryParseElementId(value) !== null && !citations.has(value)) {
      citations.set(value, { entity: value })
    }
    return
  }
  if (Array.isArray(value)) {
    for (const item of value) visit(item, citations)
    return
  }
  if (typeof value === 'object' && value !== null) {
    for (const item of Object.values(value)) visit(item, citations)
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/** `kip://profiles/cognitive-memory@2.0.0/Person` → `Person`. */
function localName(symbolRef: string): string {
  return symbolRef.slice(symbolRef.lastIndexOf('/') + 1)
}

// --- gates ------------------------------------------------------------------

function clauseName(clause: MutationClause): string {
  return Object.keys(clause)[0] ?? 'unknown'
}

/** `CreateConcept` → `CREATE CONCEPT`, for a message a model can act on. */
function spelling(name: string): string {
  return name.replace(/([a-z])([A-Z])/g, '$1 $2').toUpperCase()
}

/**
 * The state a `TRANSITION` names, resolving a `:parameter` against the
 * operation's own bindings.
 *
 * `undefined` means the gate cannot know — an unbound parameter, or one bound
 * to something that is not a string — which the caller treats as a refusal
 * rather than as permission. Otherwise the collapse into one statement would
 * have handed Formation a binding-shaped way to tombstone.
 */
function transitionState(
  scalar: Scalar,
  parameters: JsonMap | undefined,
): string | undefined {
  if ('Literal' in scalar) {
    const literal = scalar.Literal
    if (typeof literal === 'object' && literal !== null && 'String' in literal) {
      return literal.String
    }
    return undefined
  }
  const bound = parameters?.[scalar.Param]
  return typeof bound === 'string' ? bound : undefined
}

/**
 * A clause that selects with `WHERE` must say how much it may select.
 *
 * `UPDATE ?e SET ATTRIBUTES {…} WHERE { ?e CONCEPT {} }` and
 * `TRANSITION ?e TO "archived" WHERE { ?e CONCEPT {} }` are the same hazard
 * wearing two verbs, so the bound is on the selection rather than on `UPDATE`
 * by name.
 */
function assertBoundedSelection(
  clause: MutationClause,
  parameters: JsonMap | undefined,
  max: number,
): void {
  const body = Object.values(clause)[0] as
    | { where_clauses?: unknown; limit?: Scalar | null }
    | undefined
  if (!body || body.where_clauses === null || body.where_clauses === undefined) return
  if (!Array.isArray(body.where_clauses) || body.where_clauses.length === 0) return

  // `MERGE CONCEPT` is the one selecting clause KIP gives no `LIMIT`: its
  // grammar has no slot for one, so `limit` is absent rather than null. That
  // is not a hole. Its `WHERE` is a guard, not a selector — both engines
  // resolve each operand separately and refuse an operand that binds more than
  // one Concept, naming the count and asking for a stable identity. Refusing
  // every guarded merge here instead would cost the legitimate one-pair case
  // and buy an error worse than the engine's own.
  if (body.limit === undefined) return

  const limit = scalarInteger(body.limit ?? null, parameters)
  if (limit === undefined || limit > max) {
    throw new Error(
      `a ${spelling(clauseName(clause))} that selects with WHERE must use LIMIT ${max} or less`,
    )
  }
}

function assertBoundedRead(
  command: Command,
  parameters: JsonMap | undefined,
  maxResults: number,
): void {
  if ('Kml' in command) {
    throw new Error('read-only KIP accepts only KQL and META commands')
  }
  if ('Kql' in command) {
    return assertLimit(command.Kql.limit, parameters, maxResults)
  }
  return assertBoundedMeta(command.Meta, parameters, maxResults)
}

function assertBoundedMeta(
  command: MetaCommand,
  parameters: JsonMap | undefined,
  maxResults: number,
): void {
  if ('List' in command) return assertLimit(command.List.limit, parameters, maxResults)
  if ('Search' in command) return assertLimit(command.Search.limit, parameters, maxResults)
  if ('History' in command) {
    const body = 'Element' in command.History ? command.History.Element : command.History.Space
    return assertLimit(body.limit, parameters, maxResults)
  }
  if ('Changes' in command) {
    const body = 'Since' in command.Changes ? command.Changes.Since : command.Changes.AfterSeq
    return assertLimit(body.limit, parameters, maxResults)
  }
  if ('ExportCapsule' in command) {
    // A capsule is a signed subgraph export, not evidence for one answer.
    throw new Error('model read plans cannot export capsules')
  }
  if ('Preview' in command) {
    // PREVIEW takes a KML string. It writes nothing, but a read plan carrying
    // mutation text past the read-only gate is exactly what the gate is for.
    throw new Error('model read plans cannot preview KML')
  }
  // DESCRIBE (including `DESCRIBE SNAPSHOT`, which absorbed the standalone
  // `SNAPSHOT` command), VALIDATE and VERIFY are bounded metadata reads.
}

function assertLimit(
  limit: Scalar | null,
  parameters: JsonMap | undefined,
  maxResults: number,
): void {
  const value = scalarInteger(limit, parameters)
  if (value === undefined || value > maxResults) {
    throw new Error(`model read commands must use LIMIT ${maxResults} or less`)
  }
}

/**
 * The integer a value slot holds, resolving a `:parameter` against the
 * operation's own bindings.
 *
 * Refusing every parameterised limit would push a model toward splicing the
 * number into the command text, which is the habit binding exists to break.
 */
function scalarInteger(
  scalar: Scalar | null,
  parameters: JsonMap | undefined,
): number | undefined {
  if (scalar === null) return undefined
  if ('Literal' in scalar) {
    const literal = scalar.Literal
    if (typeof literal === 'object' && literal !== null && 'Number' in literal) {
      return integer(literal.Number)
    }
    return undefined
  }
  return integer(parameters?.[scalar.Param])
}

function integer(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isInteger(value) && value > 0
    ? value
    : undefined
}

/** Facets a model plan cannot write: learning records and runtime state belong
 * to host bindings, and `GradingState` is a computed view of `current_evaluation`. */
const PROTECTED_FACETS = ['TrialRecord', 'EvaluationRecord', 'OutcomeRecord', 'AttemptRecord',
  'GradingState', 'WatchState', 'LeaseState']

/** Structural fields a model plan cannot write: the Skill's learning pointers
 * move only with host trial/verdict transactions, and lineage is computed. */
const PROTECTED_STRUCTURAL = ['current_trial', 'current_evaluation', 'derived_from',
  'compiled_from', 'compiled_by', 'consolidated_to']

/** Check facet and structural AST positions, never words inside captured source payloads. */
function assertCognitiveRecords(clause: MutationClause, parameters?: JsonMap): void {
  const body = Object.values(clause)[0] as Record<string, unknown>
  const facets: unknown[] = []
  const structural: unknown[] = []
  for (const key of ['set_facets', 'unset_facets']) {
    const entries = body[key]
    if (Array.isArray(entries)) for (const entry of entries) facets.push(entry.facet)
  }
  for (const key of ['set_structural', 'unset_structural']) {
    const entries = body[key]
    if (Array.isArray(entries)) for (const entry of entries) structural.push(entry.field)
  }
  if (Array.isArray(body.actions)) for (const action of body.actions) {
    if ('SetFacet' in action) facets.push(action.SetFacet.facet)
    if ('UnsetFacet' in action) facets.push(action.UnsetFacet.facet)
    for (const key of ['SetStructural', 'UnsetStructural']) {
      if (key in action && Array.isArray(action[key])) for (const edge of action[key]) structural.push(edge.field)
    }
  }
  const resolve = (value: unknown): unknown => {
    const symbol = value as { Name?: string; Param?: string }
    return symbol.Name ?? (symbol.Param ? parameters?.[symbol.Param] : undefined)
  }
  for (const facet of facets) {
    const name = resolve(facet)
    if (typeof name !== 'string') throw new Error('model facet names must resolve before execution')
    if (PROTECTED_FACETS.includes(localName(name))) {
      throw new Error(`UnsupportedCapability: ${name} requires a configured host learning/runtime binding or is computed by the engine; it cannot be authored by a model plan`)
    }
  }
  for (const field of structural) {
    const name = resolve(field)
    if (typeof name !== 'string') throw new Error('model structural field names must resolve before execution')
    if (PROTECTED_STRUCTURAL.includes(localName(name))) {
      throw new Error(`UnsupportedCapability: ${name} is moved by host learning transactions or computed from Activity provenance; it cannot be authored by a model plan`)
    }
  }
}
