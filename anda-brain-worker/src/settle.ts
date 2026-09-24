/** Bounded maintenance. Nexus owns Watch generations and authorized coverage.
 * No local outcome-count rule can confer validated Skill standing. */
import { KipError, isJsonMap, type JsonMap, type KipResult } from '@ldclabs/kip-do'
import type { KipOperation } from './kip.js'
import type {
  ArmedWatch,
  CorrectionScan,
  CorrectionCursor,
  Dependent,
  MaintenanceAssessment,
  RevisedRoot,
  SettlementReport,
  WatchSettlement,
} from './types.js'


export type RunKip = (operation: KipOperation) => KipResult
export type AdvanceWatch = (id: string, version: number, generation: number) => JsonMap

export interface SettlePosition {
  pendingCorrections?: CorrectionScan
  correctionCursor?: number | CorrectionCursor
  advanceWatch?: AdvanceWatch
}

/**
 * There is no disuse sweep: decay is computed from base, anchor and a pinned
 * strength policy when a read is evaluated (Spec §59.1, Profile §6.1, §18),
 * and a missing input leaves strength unknown.
 */
export function settle(run: RunKip, nowMs: number, position: SettlePosition = {}): SettlementReport {
  const now = new Date(nowMs).toISOString()
  return {
    settled_at: now,
    watches: sweepWatches(run, position.advanceWatch),
    skills: {
      graded: 0, transitions: 0, conflicted: 0,
      unsupported_reason: 'memory_learning requires configured independent observers, frozen trials and replayable evaluations',
    },
    corrections: position.pendingCorrections ?? scanCorrections(run, position.correctionCursor ?? 0),
  }
}

export function assess(
  run: RunKip,
  spaceSeq: number,
  extras: { revisedRoots?: RevisedRoot[] } = {},
): MaintenanceAssessment {
  return {
    space_seq: spaceSeq,
    armed_watches: watchesInStatus(run, 'armed'),
    fired_watches: watchesInStatus(run, 'fired'),
    predicates: predicateCensus(run),
    revised_roots: extras.revisedRoots ?? [],
  }
}


const WATCH_SWEEP_LIMIT = 20

function sweepWatches(run: RunKip, advance?: AdvanceWatch): WatchSettlement {
  const report: WatchSettlement = { fired: 0, conflicted: 0, disarmed: 0, deferred: 0 }
  const found = run(watchesCommand('armed'))
  if (found.status === 'failed') {
    report.error = found.error?.message ?? 'watch scan failed'
    return report
  }
  const rows = readWatchRows(found.result)
  let runnable: WatchRow[] = []
  for (const row of rows) {
    if (row.generation === undefined) {
      report.deferred += 1
      report.error = 'armed Watch has no WatchState; review its observation gap and re-arm it through the host'
      continue
    }
    // An arbitrary text member must never be ignored by a structured matcher.
    if (!isObject(row.condition) || 'text' in row.condition ||
        !['element', 'slot', 'type'].some((key) => key in (row.condition as Record<string, unknown>))) {
      report.deferred += 1
      continue
    }
    runnable.push(row)
  }
  if (rows.length >= WATCH_SWEEP_LIMIT) {
    const selected = run(watchesCommand('armed', true))
    if (selected.status === 'failed') {
      report.error = selected.error?.message ?? 'runnable watch scan failed'
      return report
    }
    runnable = readWatchRows(selected.result)
  }
  for (const row of runnable) {
    if (row.generation === undefined || !isObject(row.condition) || 'text' in row.condition) continue
    if (!advance) {
      report.deferred += 1
      report.error = 'Watch runtime is unavailable'
      continue
    }
    try {
      const result = advance(row.id, row.version, row.generation)
      if (result.status === 'fired') report.fired += 1
      else if (result.status === 'expired' || result.status === 'disarmed') report.disarmed += 1
      else report.deferred += 1
    } catch (error) {
      const failure = KipError.from(error)
      if (failure.code === 'VersionConflict') report.conflicted += 1
      else report.deferred += 1
      report.error = failure.message
    }
  }
  return report
}

interface WatchRow {
  id: string
  version: number
  generation?: number
  condition: unknown
  watch: ArmedWatch
}

function watchesCommand(status: string, runnable = false): KipOperation {
  const filter = runnable
    ? 'FILTER(IS_NOT_NULL(?w.facets["WatchState"].arm_generation)) FILTER(IS_NULL(?w.attributes.condition.text)) ' +
      'FILTER(IS_NOT_NULL(?w.attributes.condition.element) || IS_NOT_NULL(?w.attributes.condition.slot) || IS_NOT_NULL(?w.attributes.condition.type))'
    : ''
  return {
    command: 'FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version, ?w.facets["WatchState"], ?w.schema_ref) ' +
      `WHERE { ?w CONCEPT {type: "Watch"} FILTER(?w.attributes.status == :status) ${filter} } ORDER BY ?w.updated_at LIMIT ${WATCH_SWEEP_LIMIT}`,
    parameters: { status },
  }
}

function readWatchRows(result: unknown): WatchRow[] {
  if (!Array.isArray(result)) return []
  return result.flatMap((row): WatchRow[] => {
    if (!Array.isArray(row)) return []
    const [id, name, attributes, version, state, schemaRef] = row
    if (typeof id !== 'string' || typeof version !== 'number' || !isObject(attributes)) return []
    const text = (key: string): string => typeof attributes[key] === 'string'
      ? attributes[key] : attributes[key] === undefined ? '' : JSON.stringify(attributes[key])
    return [{ id, version, condition: attributes.condition,
      generation: isObject(state) && typeof state.arm_generation === 'number' ? state.arm_generation : undefined,
      watch: { id, ...(typeof schemaRef === 'string' ? {schema_ref:schemaRef} : {}), version,
        name: typeof name === 'string' ? name : '', watch_class: text('watch_class'),
        condition: text('condition'), summary: text('summary'), due_at: text('due_at') },
    }]
  })
}

/** An element id, whether the row spelled it bare or as a reference. */
function elementId(value: unknown): string | undefined {
  if (typeof value === 'string') return value
  if (isObject(value) && typeof value.id === 'string') return value.id
  return undefined
}

// --- correction discovery ---------------------------------------------------

/** How many superseded Assertions one scan reads. */
const CORRECTION_SCAN_LIMIT = 20

/** How far a derivation walk follows Activity lineage from a revised root. */
const DEPENDENTS_DEPTH = 2

/** How many dependents one walk lists. */
const DEPENDENTS_LIMIT = 20

/**
 * Correction discovery: the Assertions an actor has revised since `after`,
 * each with what `LIST DEPENDENTS` reaches from it (§57.5, §63.5).
 *
 * A durable (space_seq, assertion_id) cursor resumes within a transaction.
 * Each cycle reads a bounded page; a full page reports incomplete coverage,
 * never skips its unread tail. Old scalar checkpoints remain readable.
 *
 * Reachability is topology, not judgment: a listed dependent is a candidate
 * for a `review_derived` SleepTask; its currentness is the engine's computed
 * `_system.dependency_validity`. The cycle decides.
 */
function scanCorrections(run: RunKip, position: number | CorrectionCursor): CorrectionScan {
  const after = typeof position === 'number' ? { seq: position, after_id: '' } : position
  const scan: CorrectionScan = { revised_roots: [], cursor: after.seq, incomplete: false,
    ...(after.after_id ? { cursor_after_id: after.after_id } : {}),
  }
  const found = run(supersededCommand(after))
  if (found.status === 'failed') {
    scan.incomplete = true
    scan.error = found.error?.message ?? 'correction scan failed'
    return scan
  }
  if (!Array.isArray(found.result)) {
    scan.incomplete = true
    scan.error = 'correction scan returned no row array'
    return scan
  }
  const rows = found.result
  const roots: RevisedRoot[] = []
  for (const row of rows) {
    if (!Array.isArray(row)) continue
    const [id, seq, actor, proposition, supersededBy] = row
    if (typeof id !== 'string' || typeof seq !== 'number') continue
    const actorId = elementId(actor)
    const propositionId = elementId(proposition)
    roots.push({
      assertion: id,
      space_seq: seq,
      ...(actorId === undefined ? {} : { actor: actorId }),
      ...(propositionId === undefined ? {} : { proposition: propositionId }),
      superseded_by: Array.isArray(supersededBy)
        ? supersededBy.map(elementId).filter((v): v is string => v !== undefined)
        : [],
      dependents: [],
      truncated: false,
    })
  }
  if (roots.length !== rows.length) {
    scan.incomplete = true
    scan.error = 'correction scan returned an unreadable row; cursor retained'
    return scan
  }
  scan.incomplete = rows.length >= CORRECTION_SCAN_LIMIT || found.next_cursor !== undefined
  const selected = roots.filter((root) => root.space_seq > after.seq ||
    root.space_seq === after.seq && after.after_id !== '' && root.assertion > after.after_id)
    .sort((a,b) => a.space_seq - b.space_seq || (a.assertion < b.assertion ? -1 : a.assertion > b.assertion ? 1 : 0))
  const last = selected.at(-1)
  if (last) {
    scan.cursor = last.space_seq
    if (scan.incomplete) scan.cursor_after_id = last.assertion
    else delete scan.cursor_after_id
  } else if (!scan.incomplete) delete scan.cursor_after_id
  for (const root of selected) {
    const walked = run(dependentsCommand(root.assertion))
    if (walked.status === 'failed') {
      root.truncated = true
    } else {
      root.dependents = readDependents(walked.result)
      root.truncated =
        walked.next_cursor !== undefined ||
        walked.extensions?.['kip-do/dependents']?.truncated === true
    }
    scan.revised_roots.push(root)
  }
  return scan
}

function supersededCommand(after: CorrectionCursor): KipOperation {
  const boundary = after.after_id === '' ? 'FILTER(?a._system.space_seq > :after)' :
    'FILTER(?a._system.space_seq > :after || (?a._system.space_seq == :after && ?a.id > :after_id))'
  return {
    command:
      'FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition, ?a.lifecycle.superseded_by) WHERE { ' +
      '?a ASSERTION {} ' +
      'FILTER(?a.lifecycle.status == "superseded") ' +
      boundary +
      ` } ORDER BY ?a._system.space_seq, ?a.id LIMIT ${CORRECTION_SCAN_LIMIT}`,
    parameters: { after: after.seq, after_id: after.after_id },
  }
}

function dependentsCommand(root: string): KipOperation {
  return {
    command: `LIST DEPENDENTS :root DEPTH ${DEPENDENTS_DEPTH} LIMIT ${DEPENDENTS_LIMIT}`,
    parameters: { root },
  }
}

/** The rows of a `LIST DEPENDENTS` answer — `{id, kind, distance, via}`. */
function readDependents(result: unknown): Dependent[] {
  if (!Array.isArray(result)) return []
  const dependents: Dependent[] = []
  for (const row of result) {
    if (!isObject(row) || typeof row.id !== 'string') continue
    const via = isObject(row.via) && typeof row.via.activity === 'string' ? row.via.activity : undefined
    dependents.push({
      id: row.id,
      kind: typeof row.kind === 'string' ? row.kind : '',
      distance: typeof row.distance === 'number' ? row.distance : 1,
      ...(via === undefined ? {} : { via }),
    })
  }
  return dependents
}


// --- assessment reads -------------------------------------------------------

/**
 * The Watches this Space holds in one status.
 *
 * A failed read answers an empty set: the assessment is a signal block, and one
 * signal it could not gather must not take the cycle down with it.
 */
function watchesInStatus(run: RunKip, status: string): ArmedWatch[] {
  const result = run(watchesCommand(status))
  return result.status === 'failed' ? [] : readWatchRows(result.result).map((row) => row.watch)
}

/**
 * Per-predicate link counts — the vocabulary sprawl indicator.
 *
 * A count that failed is omitted rather than reported as zero: naming the
 * busiest predicate as unused would point the merge guidance at exactly the
 * wrong target.
 */
function predicateCensus(run: RunKip): Record<string, number> {
  const listed = run({ command: 'LIST PREDICATES LIMIT 1000' })
  const counted = run({ command: 'FIND(?predicate, COUNT(?link)) WHERE { ?link (?s, ?predicate, ?o) } LIMIT 1000' })
  if (listed.status !== 'succeeded' || counted.status !== 'succeeded' || counted.next_cursor ||
      !Array.isArray(listed.result) || !Array.isArray(counted.result)) return {}
  // Native counts retain visibility. Merge exact-version groups by lineage,
  // then expose only unambiguous active local names, including zero-use symbols.
  const lineage = (ref: string) => ref.replace(/@[^/]+(?=\/[^/]+$)/, '')
  const counts = new Map<string, number>()
  for (const row of counted.result) {
    if (!Array.isArray(row) || typeof row[0] !== 'string' || typeof row[1] !== 'number') return {}
    const key = lineage(row[0])
    counts.set(key, (counts.get(key) ?? 0) + row[1])
  }
  const entries = listed.result.filter(isJsonMap)
  const census: Record<string, number> = {}
  for (const entry of entries) {
    const name = entry.local_name, ref = entry.ref
    if (typeof name === 'string' && typeof ref === 'string' &&
        entries.filter(candidate => candidate.local_name === name).length === 1) {
      census[name] = counts.get(lineage(ref)) ?? 0
    }
  }
  return census
}

// --- helpers ----------------------------------------------------------------

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
