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
  type MutationClause,
  type Outcome,
  type Scalar,
} from '@ldclabs/kip-do'
import type { MemoryCitation } from './types.js'

export const MAX_KIP_OPERATIONS = 4
export const MAX_KIP_COMMAND_BYTES = 256 * 1024

/** The most rows a model-planned read may ask for. */
export const MAX_MODEL_RESULTS = 20

/** The most elements one maintenance clause may select. */
export const MAX_MAINTENANCE_SELECTION = 20

/** The most citations one answer carries. */
const MAX_CITATIONS = 16

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
 * `SUPERSEDE` and `RETRACT` belong here even though they change a claim's
 * standing.
 *
 * What is missing is deliberate. `UPDATE`, `ARCHIVE`, `TOMBSTONE`, `PURGE` and
 * `MERGE` act on memory in bulk from a selection, and a formation pass reading
 * an untrusted conversation is the last thing that should hold them.
 */
const FORMATION_CLAUSES = new Set([
  'CreateConcept',
  'UpsertConcept',
  'EnsureProposition',
  'CreateEvidence',
  'CreateAssertion',
  'CreateActivity',
  'RetractAssertion',
  'SupersedeAssertion',
  'CorrectEvidence',
  'TransitionActivity',
])

export function assertOperationBatch(operations: readonly KipOperation[]): void {
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
export function assertBoundedReadonlyOperations(
  operations: readonly KipOperation[],
  maxResults: number,
): void {
  assertReadonlyOperations(operations)
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
      const name = clauseName(clause)
      if (!FORMATION_CLAUSES.has(name)) {
        throw new Error(
          `formation cannot issue ${spelling(name)}; it writes cognition ` +
            '(CREATE / UPSERT / ENSURE / ASSERT) and corrects it (SUPERSEDE / RETRACT)',
        )
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
      if ('Purge' in clause || 'PurgePayload' in clause) {
        throw new Error('maintenance plans cannot issue KIP PURGE commands')
      }
      assertBuilt(clause)
      assertBoundedSelection(clause, operation.parameters, MAX_MAINTENANCE_SELECTION)
    }
  }
}

/**
 * Clauses the engine parses and then refuses to run.
 *
 * `SET RETENTION` is legal KIP that `@ldclabs/kip-do` has not implemented, so
 * it sailed through this gate and failed at execution — and because a batch is
 * not a transaction, the commands *before* it had already committed while the
 * request as a whole answered 422. Refusing at the gate makes the whole plan
 * fail before anything runs, which is the outcome a caller can act on.
 *
 * Kept as an explicit list rather than a try/catch around execution: a plan
 * rejected for naming an unbuilt capability should say so in the same breath
 * as the other gates, and the deployment contract (§A.5) already tells the
 * model this one is refused.
 */
const UNBUILT_CLAUSES = new Set(['SetRetention'])

function assertBuilt(clause: MutationClause): void {
  const name = clauseName(clause)
  if (UNBUILT_CLAUSES.has(name)) {
    throw new Error(
      `${spelling(name)} is not implemented by this engine; say what should ` +
        'expire in the summary rather than encoding a decision nothing enforces',
    )
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

/** The first failure in a batch, whichever operation it sits at. */
export function firstKipError(
  results: readonly KipResult[],
): KipResult['error'] | undefined {
  return results.find((result) => result.status === 'failed')?.error
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
 * How many elements a batch changed under the given op.
 *
 * A KML outcome is `{handles, changes: [{id, kind, op, version}, …]}`; there is
 * no scalar count, and reporting the whole `changes` length would count the
 * elements one statement touched incidentally.
 */
function countChangeOps(results: readonly KipResult[]): Record<string, number> {
  const counts: Record<string, number> = {}
  for (const result of results) {
    for (const change of outcomeOf(result)?.changes ?? []) {
      counts[change.op] = (counts[change.op] ?? 0) + 1
    }
  }
  return counts
}

/** What one write produced, in the shape the API has always reported. */
export function countChanges(results: readonly KipResult[]): {
  total: number
  created: number
  updated: number
  retired: number
  merged: number
} {
  const counts = countChangeOps(results)
  const created = counts.create ?? 0
  const updated = (counts.update ?? 0) + (counts.transition ?? 0) + (counts.retention ?? 0)
  const retired =
    (counts.archive ?? 0) +
    (counts.tombstone ?? 0) +
    (counts.retract ?? 0) +
    (counts.supersede ?? 0) +
    (counts.correct ?? 0) +
    (counts.purge ?? 0)
  const merged = counts.merge ?? 0
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
export function citationsFromLookup(result: Json | undefined): MemoryCitation[] {
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
 * Element ids anywhere in a model-planned read's results.
 *
 * A planned command projects whatever it liked, so nothing here knows which
 * column holds what. An id alone is still a usable citation — it names an
 * element a reader can go and look at — and claiming a type or a name that was
 * never projected would be worse than omitting them.
 */
export function collectCitations(
  results: readonly KipResult[],
  seed: readonly MemoryCitation[] = [],
): MemoryCitation[] {
  const citations = new Map<string, MemoryCitation>()
  for (const citation of seed) citations.set(citation.entity, citation)
  for (const result of results) {
    visit(result.result, citations)
  }
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
 * A clause that selects with `WHERE` must say how much it may select.
 *
 * `UPDATE ?e SET ATTRIBUTES {…} WHERE { ?e CONCEPT {} }` and
 * `ARCHIVE ?e WHERE { ?e CONCEPT {} }` are the same hazard wearing two verbs,
 * so the bound is on the selection rather than on `UPDATE` by name.
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

  // `MERGE CONCEPT` is the one selecting clause KIP gives no `LIMIT` — its
  // grammar has no slot for one, so `limit` is absent rather than null, and an
  // early return here let `MERGE CONCEPT ?s INTO ?t WHERE { ?s CONCEPT {} … }`
  // through as the only unbounded selection a plan could make. Merge is
  // non-destructive, so this is a blast-radius bound, not a data-loss one:
  // require the pattern to name its endpoints instead of sweeping for them.
  if (body.limit === undefined) {
    if ('MergeConcept' in clause) {
      throw new Error(
        'a MERGE CONCEPT takes no LIMIT, so its WHERE must identify exactly ' +
          'the source and the target — merge duplicates one pair at a time',
      )
    }
    return
  }

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
  // DESCRIBE, VALIDATE, VERIFY and SNAPSHOT are bounded metadata reads.
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
