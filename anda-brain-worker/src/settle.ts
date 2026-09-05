/**
 * The deterministic settlement: what the runtime decides without asking a model.
 *
 * This is the Worker's port of `anda_brain`'s settlement passes, and it exists
 * because the gap between the two deployments was never really about the
 * engine. `@ldclabs/kip-do` reads and writes everything these passes need —
 * `UPDATE` with `CLAMP`/`MUL`/`COALESCE`, `EXPECT VERSION`, `CHANGES AFTER SEQ`,
 * `LIST DEPENDENTS`. What the Worker lacked was the idea that the *host* may
 * read: its maintenance model gets one completion and emits KML, so anything
 * needing a read before a write looked impossible. It is only impossible for
 * the model. The Durable Object holds the nexus directly.
 *
 * The division is the same one `anda_brain` settled on:
 *
 * > the runtime does the arithmetic, the model does the interpretation.
 *
 * A deadline passing, a success rate crossing its recorded basis, a week of
 * disuse — none of these need a language model, and all of them were waiting
 * on one. What still needs interpretation stays with the cycle: whether a
 * committed change matches a condition written in prose, what a fired Watch
 * means, which Experiences contrast usefully enough to compile.
 *
 * # Keeping the two engines honest
 *
 * Every threshold here is duplicated from `anda_brain/src/{watch,skill}.rs`,
 * because two implementations of one rule that disagree are worse than one
 * implementation and a gap: a Skill adopted by the Rust deployment and revoked
 * by the Worker would make `VERDICT_RULE` a lie. The constant is shared
 * deliberately — it names the *rule*, not the implementation, and both engines
 * must move it together or not at all.
 */

import type { JsonMap, KipResult } from '@ldclabs/kip-do'
import type { KipOperation } from './kip.js'
import type {
  ArmedWatch,
  CorrectionScan,
  Dependent,
  MaintenanceAssessment,
  RevisedRoot,
  SettlementReport,
  SkillSettlement,
  WatchSettlement,
} from './types.js'

/**
 * The one thing settlement needs from its host: run a command, get a result.
 *
 * Everything below is arithmetic over what the graph already holds, so the only
 * capability it cannot supply itself is execution. Taking it as a port rather
 * than reaching for a Durable Object is what lets the whole settlement — the
 * verdict rule most of all — be exercised without one.
 *
 * Synchronous, because the 2.0 engine is: a Durable Object runs a command
 * against its own SQLite without awaiting, and a promise here would buy a
 * suspension point in the middle of a sweep that reads its own writes.
 */
export type RunKip = (operation: KipOperation) => KipResult

/**
 * The deterministic settlement, run before every maintenance cycle.
 *
 * Never throws and never fails the cycle: a settlement that could not sweep is
 * a degraded cycle, not a failed one, and each pass reports its own error so
 * "nothing was due" and "the pass never ran" stay distinguishable.
 */
export function settle(run: RunKip, nowMs: number, position: SettlePosition = {}): SettlementReport {
  const now = new Date(nowMs).toISOString()
  return {
    settled_at: now,
    decayed: metabolize(run, now, new Date(nowMs - DECAY_MIN_INTERVAL_MS).toISOString()),
    watches: sweepWatches(run, now, position),
    skills: settleSkillLifecycle(run, now),
    corrections: scanCorrections(run, position.correctionCursor ?? 0),
  }
}

/** Where the settlement reads from: the host's coordinates, which no command answers. */
export interface SettlePosition extends StreamPosition {
  /** The coordinate the last correction scan read past. */
  correctionCursor?: number
}

/**
 * What the settlement measured, as the maintenance prompt receives it.
 *
 * The model cannot go and fetch any of this — it gets one completion — so a
 * signal absent here is a duty it will not perform. That is why the armed set,
 * the fired queue and `space_seq` are read for it rather than left to a §6
 * assessment it has no way to run.
 *
 * `spaceSeq` is a parameter rather than a read: it is the store's own
 * coordinate, not something any command answers. So are `consumedSeq` — the
 * host's record of what the last cycle read — and `revisedRoots`, which the
 * settlement just found.
 */
export function assess(
  run: RunKip,
  spaceSeq: number,
  extras: { consumedSeq?: number; revisedRoots?: RevisedRoot[] } = {},
): MaintenanceAssessment {
  return {
    space_seq: spaceSeq,
    ...(extras.consumedSeq === undefined ? {} : { consumed_seq: extras.consumedSeq }),
    armed_watches: watchesInStatus(run, 'armed'),
    fired_watches: watchesInStatus(run, 'fired'),
    predicates: predicateCensus(run),
    revised_roots: extras.revisedRoots ?? [],
  }
}

// --- rule constants, mirrored from anda_brain -------------------------------

/**
 * The identity of the Skill verdict rule, recorded on every verdict.
 *
 * Identical to `anda_brain::skill::VERDICT_RULE` on purpose: an auditor
 * recomputing a verdict must get the same answer whichever deployment wrote it.
 * Bump both together, never one.
 */
export const VERDICT_RULE = 'anda-brain/skill-verdict@2'

/** Graded outcomes a trial needs before any verdict may move it. */
const TRIAL_MIN_OUTCOMES = 5

/** How far the trial's rate must beat (or trail) its basis — one margin, both directions. */
const VERDICT_MARGIN = 0.1

/** `OutcomeRecord.magnitude` at or above which one failure may revoke alone. */
const HIGH_SEVERITY = 0.8

/** How many Outcome Evidence records one verdict pass reads per Skill. */
const OUTCOME_WINDOW = 200

/** How many Skills one pass evaluates. */
const SKILL_SCAN_LIMIT = 50

/** How many Watches one sweep may fire. */
const WATCH_SWEEP_LIMIT = 20

/** `MnemonicState.memory_strength` assumed for a Concept that has never been metabolized. */
const DEFAULT_MEMORY_STRENGTH = 0.5

/** Multiplier disuse metabolism applies per sweep. */
const DECAY_FACTOR = 0.95

/** Lower bound disuse metabolism may not push `memory_strength` below. */
const DECAY_FLOOR = 0.3

/** Concepts metabolized more recently than this are skipped: the decay is weekly. */
const DECAY_MIN_INTERVAL_MS = 7 * 24 * 3_600 * 1_000

/** Per-command row limit for the bulk sweep. */
const DECAY_BATCH_LIMIT = 200

/**
 * The deployment-local Skill attribute; see `anda_brain::skill`.
 *
 * The one piece of bookkeeping no Profile facet has a slot for: the highest
 * outcome coordinate already counted. `TrialState.basis_seq` is a different
 * number — where the *trial* opened — so it cannot stand in for this one.
 */
const CURSOR_ATTR = 'verdict_cursor'

// --- watch evaluation --------------------------------------------------------

/** How many Change Envelopes one stream page carries. */
const CHANGES_PAGE_LIMIT = 200

/**
 * How many pages one sweep reads before it stops and records where it got
 * to. A Space that moved further than this between two cycles is evaluated
 * across several sweeps rather than in one unbounded read.
 */
const CHANGES_MAX_PAGES = 5

/** The columns every Watch scan projects, in the order the readers use them. */
const WATCH_COLUMNS = '?w.id, ?w.name, ?w.attributes, ?w._system.plane_versions.attributes'

/** Where the sweep reads the stream from and what the Brain has consumed. */
export interface StreamPosition {
  /** The Space's head; absent when the store could not answer. */
  headSeq?: number
  /** The coordinate the last completed cycle read the stream through. */
  consumedSeq?: number
}

/**
 * Evaluates the armed Watches (Profile §5.11).
 *
 * Two halves, and the split is what this function is:
 *
 * - A **structured** condition — `element`, `slot` or `type`, narrowed by
 *   `ops` and `touched` — is the runtime's to read. The sweep reads the
 *   Change Stream from where each Watch was last evaluated through the head
 *   and matches the entries: a delta Watch fires on the first match, a silence
 *   Watch whose awaited change arrived stands down, and a silence Watch past
 *   its deadline with nothing matched fires — silence concluded over a stream
 *   consumed through the head, which is what §5.11 means by it.
 * - A **prose** condition is the Brain's. The runtime cannot say whether "no
 *   reply from the vendor" was answered, so a prose silence Watch past its
 *   deadline fires only once the Brain has consumed the stream through the
 *   head at which the sweep first saw the deadline passed: the clock alone
 *   proves nothing, because a matching change committed before the deadline
 *   may still be waiting for the model whose job it is to read it.
 *
 * One scan, then one guarded write per Watch. A write refused by
 * `EXPECT VERSION` is counted as a conflict and left where it was rather than
 * retried: something else moved that Watch between the scan and the write, and
 * the next sweep re-reads it.
 */
function sweepWatches(run: RunKip, now: string, stream: StreamPosition): WatchSettlement {
  const report: WatchSettlement = { fired: 0, conflicted: 0, disarmed: 0, deferred: 0 }
  const evaluated = new Set<string>()

  // The armed set, evaluated wherever the condition is the runtime's to read.
  // Read whole rather than filtered in KQL: whether a condition is structured
  // is a question about its shape, which is this side's to ask.
  const armed = run(watchesCommand('armed'))
  if (armed.status === 'failed') {
    report.error = armed.error?.message ?? 'watch scan failed'
    return report
  }
  const structured: [WatchRow, ChangeFilter][] = []
  for (const row of readWatchRows(armed.result)) {
    const filter = changeFilter(row.condition)
    if (filter !== null) structured.push([row, filter])
  }
  if (structured.length > 0) {
    if (stream.headSeq === undefined) {
      console.warn('the Space head is unknown; structured Watches are not evaluated this cycle')
    } else {
      const error = evaluateStructured(
        run,
        now,
        { headSeq: stream.headSeq, consumedSeq: stream.consumedSeq },
        structured,
        evaluated,
        report,
      )
      if (error !== null) {
        report.error = error
        return report
      }
    }
  }

  // The silence Watches past their deadline that evaluation did not settle:
  // prose conditions, whose consumption is the Brain's. A structured Watch
  // lands here only when the head was unknown, and is then held to the same
  // guard.
  const due = run(dueSilenceWatchesCommand(now))
  if (due.status === 'failed') {
    report.error = due.error?.message ?? 'watch scan failed'
    return report
  }
  for (const row of readWatchRows(due.result)) {
    if (evaluated.has(row.id)) continue
    let command: KipOperation
    if (row.dueSeenSeq !== undefined) {
      // The Brain has read the stream past the head at which this deadline
      // was first seen passed: silence is a fact now. Until then, held.
      if (stream.consumedSeq === undefined || stream.consumedSeq < row.dueSeenSeq) {
        report.deferred += 1
        continue
      }
      command = fireWatchCommand(row, now, { kind: 'silence' }, stream.consumedSeq)
    } else if (stream.headSeq !== undefined) {
      // First sight of the passed deadline: record the head, so the
      // consumption the next cycle reaches can be measured against it.
      command = stampCommand(row, 'due_seen_seq', stream.headSeq)
    } else {
      report.deferred += 1
      continue
    }
    const firing = row.dueSeenSeq !== undefined
    if (run(command).status === 'failed') report.conflicted += 1
    else if (firing) report.fired += 1
    else report.deferred += 1
  }
  return report
}

/** What one Watch write meant, for the report. */
type Outcome = 'fired' | 'disarmed' | 'deferred' | 'evaluated'

/**
 * Evaluates the structured Watches against the Change Stream from the oldest
 * coordinate any of them needs, through the head.
 *
 * One stream read serves every Watch, and each is matched only past its own
 * start, so a Watch armed yesterday is not fired by a change committed last
 * week. The start is `evaluated_seq` where a sweep has stamped one; otherwise
 * it is the Watch's arming, found in the stream itself as the last entry that
 * created it or touched its attributes. A Watch is armed by the maintenance
 * model, after the cycle's `space_seq` — so a Watch not yet stamped was armed
 * past `consumedSeq`, which is where the stream is read from for it, and where
 * it starts when its arming is not in the window after all. Not the Watch's
 * own `_system.space_seq`: the metabolism sweep writes a Facet on every
 * Concept, Watches included, and that coordinate would follow the sweep to
 * the head and step over the very changes the Watch was armed for.
 *
 * The ids evaluated are collected so the due sweep does not settle them a
 * second time on a version this pass already moved.
 */
function evaluateStructured(
  run: RunKip,
  now: string,
  stream: StreamPosition & { headSeq: number },
  watches: readonly [WatchRow, ChangeFilter][],
  evaluated: Set<string>,
  report: WatchSettlement,
): string | null {
  const head = stream.headSeq
  const tentative = stream.consumedSeq ?? 0
  const from = Math.min(head, ...watches.map(([row]) => row.evaluatedSeq ?? tentative))
  const read = readChanges(run, from, head)
  if ('error' in read) return read.error
  const { envelopes, consumedTo } = read

  // The slots the filters name, resolved once: an Assertion entry carries only
  // `refs.proposition`, so matching it against a slot needs the Propositions
  // of that slot.
  const slots: SlotIndex = new Map()
  for (const [, filter] of watches) {
    if (filter.slot === undefined) continue
    const key = slotKey(filter.slot)
    if (slots.has(key)) continue
    const found = run(slotCommand(filter.slot.subject, filter.slot.predicate))
    if (found.status === 'failed') return found.error?.message ?? 'slot lookup failed'
    const propositions = new Set<string>()
    if (Array.isArray(found.result)) {
      for (const row of found.result) {
        const id = elementId(row)
        if (id !== undefined) propositions.add(id)
      }
    }
    slots.set(key, propositions)
  }

  for (const [row, filter] of watches) {
    evaluated.add(row.id)
    const start = row.evaluatedSeq ?? armedAt(envelopes, row.id) ?? tentative
    const through = Math.max(consumedTo, start)
    const dueSilence = isSilence(row) && isDue(row, now)
    const matched = firstMatch(filter, start, envelopes, slots)
    let command: KipOperation
    let outcome: Outcome
    if (matched !== undefined && isSilence(row)) {
      command = disarmMatchedCommand(row, now, matched)
      outcome = 'disarmed'
    } else if (matched !== undefined) {
      command = fireWatchCommand(row, now, { kind: 'delta', seq: matched })
      outcome = 'fired'
    } else if (dueSilence && through >= head) {
      // Nothing matched, and the stream is consumed through the head: for a
      // due silence Watch that is silence, as §5.11 means it.
      command = fireWatchCommand(row, now, { kind: 'silence' }, through)
      outcome = 'fired'
    } else if (through <= start) {
      // Nothing new to read; nothing to record.
      if (dueSilence) report.deferred += 1
      continue
    } else {
      command = stampCommand(row, 'evaluated_seq', through)
      outcome = dueSilence ? 'deferred' : 'evaluated'
    }
    if (run(command).status === 'failed') {
      report.conflicted += 1
    } else if (outcome === 'fired') {
      report.fired += 1
    } else if (outcome === 'disarmed') {
      report.disarmed += 1
    } else if (outcome === 'deferred') {
      report.deferred += 1
    }
  }
  return null
}

/**
 * Reads the Change Stream after `from`, page by page, within the sweep's
 * budget.
 *
 * Answers the envelopes and the coordinate they are complete through: the head
 * when the stream was read to its end, or the last coordinate read when the
 * budget ran out first — which the caller records, so the next sweep continues
 * from there instead of concluding silence over changes it never saw.
 */
function readChanges(
  run: RunKip,
  from: number,
  head: number,
): { envelopes: unknown[]; consumedTo: number } | { error: string } {
  let after = from
  const envelopes: unknown[] = []
  for (let page = 0; page < CHANGES_MAX_PAGES; page += 1) {
    const result = run(changesCommand(after))
    if (result.status === 'failed') {
      return { error: result.error?.message ?? 'change stream read failed' }
    }
    const rows = Array.isArray(result.result) ? result.result : []
    let last: number | undefined
    for (const envelope of rows) {
      const seq = isObject(envelope) ? envelope.space_seq : undefined
      if (typeof seq === 'number' && (last === undefined || seq > last)) last = seq
    }
    envelopes.push(...rows)
    if (last === undefined) return { envelopes, consumedTo: Math.max(after, head) }
    if (rows.length < CHANGES_PAGE_LIMIT) return { envelopes, consumedTo: Math.max(last, head) }
    after = last
  }
  return { envelopes, consumedTo: after }
}

/** One Watch, as the scan returns it. */
interface WatchRow {
  id: string
  /** The attributes-plane version the guarded writes expect. */
  version: number
  /** The condition as written — structured or prose. */
  condition: unknown
  /** Through which coordinate the runtime has evaluated this Watch. */
  evaluatedSeq?: number
  /**
   * The head at which a sweep first saw a prose silence Watch's deadline
   * passed; the guard the Brain's consumption has to reach.
   */
  dueSeenSeq?: number
  /** The Watch as the maintenance prompt receives it. */
  watch: ArmedWatch
}

function isSilence(row: WatchRow): boolean {
  return row.watch.watch_class === 'silence'
}

/** Whether the deadline has passed, as the scan compares it. */
function isDue(row: WatchRow, now: string): boolean {
  return row.watch.due_at !== '' && row.watch.due_at <= now
}

/**
 * The coordinate at which the stream last shows the Watch being armed: the
 * entry that created it, or the last one that touched its attributes — which
 * is what a model's `status: "armed"` does and a Facet sweep does not.
 */
function armedAt(envelopes: readonly unknown[], id: string): number | undefined {
  let armed: number | undefined
  for (const envelope of envelopes) {
    if (!isObject(envelope) || typeof envelope.space_seq !== 'number') continue
    const changes = Array.isArray(envelope.changes) ? envelope.changes : []
    const arming = changes.some((entry) => {
      if (!isObject(entry) || entry.id !== id) return false
      if (entry.op === 'create') return true
      const touched = Array.isArray(entry.touched) ? entry.touched : []
      return touched.some((path) => typeof path === 'string' && path.startsWith('attributes'))
    })
    if (arming && (armed === undefined || envelope.space_seq > armed)) armed = envelope.space_seq
  }
  return armed
}

/**
 * Reads scan rows into Watches.
 *
 * A row missing its id or version is skipped rather than acted on: every write
 * here is guarded, and writing without the version would overwrite a
 * concurrent edit instead of yielding to it.
 */
function readWatchRows(result: unknown): WatchRow[] {
  if (!Array.isArray(result)) return []
  const rows: WatchRow[] = []
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [id, name, attributes, version] = row
    if (typeof id !== 'string' || typeof version !== 'number') continue
    const seqAttribute = (key: string): number | undefined => {
      const value = isObject(attributes) ? attributes[key] : undefined
      return typeof value === 'number' ? value : undefined
    }
    rows.push({
      id,
      version,
      condition: isObject(attributes) ? attributes.condition : undefined,
      evaluatedSeq: seqAttribute('evaluated_seq'),
      dueSeenSeq: seqAttribute('due_seen_seq'),
      watch: readWatch(id, name, attributes),
    })
  }
  return rows
}

function readWatch(id: string, name: unknown, attributes: unknown): ArmedWatch {
  // `Watch.condition` is `string | object` since §5.11 gave the structured
  // filter a baseline form. A reader that only took the string case would
  // render a structured condition as the empty string — "this Watch declares
  // no condition", the one thing a Watch always does.
  const attribute = (key: string): string => {
    const value = isObject(attributes) ? attributes[key] : undefined
    if (typeof value === 'string') return value
    if (value === undefined || value === null) return ''
    return JSON.stringify(value)
  }
  return {
    id,
    name: typeof name === 'string' ? name : '',
    watch_class: attribute('watch_class'),
    condition: attribute('condition'),
    summary: attribute('summary'),
    due_at: attribute('due_at'),
  }
}

/**
 * A structured condition this runtime can evaluate (Profile §5.11).
 *
 * `element`, `slot` and `type` select what is watched — at least one, and
 * every one present has to hold; `ops` and `touched` narrow which entries
 * count, and an empty list means any.
 */
interface ChangeFilter {
  element?: string
  slot?: { subject: string; predicate: string }
  type?: string
  ops: string[]
  touched: string[]
}

/**
 * The runtime-evaluable reading of a condition, or `null` when it is the
 * Brain's.
 *
 * `null` for prose, for a structured form with no selector (a Watch that
 * carries only `text` is Brain-evaluated by definition), and for any member
 * this runtime cannot read the way §5.11 means it. Refusing the last case is
 * the point: a selector half-understood is a Watch that fires on the wrong
 * change or never, and either is worse than leaving it to the model.
 */
export function changeFilter(condition: unknown): ChangeFilter | null {
  if (!isObject(condition)) return null
  const filter: ChangeFilter = { ops: [], touched: [] }
  const strings = (value: unknown): string[] | null =>
    Array.isArray(value) && value.every((item) => typeof item === 'string') ? value : null
  for (const [name, value] of Object.entries(condition)) {
    switch (name) {
      case 'element':
        if (typeof value !== 'string') return null
        filter.element = value
        break
      case 'type':
        if (typeof value !== 'string') return null
        filter.type = value
        break
      case 'slot': {
        if (!isObject(value)) return null
        const { subject, predicate } = value
        if (typeof subject !== 'string' || typeof predicate !== 'string') return null
        filter.slot = { subject, predicate }
        break
      }
      case 'ops': {
        const ops = strings(value)
        if (ops === null) return null
        filter.ops = ops
        break
      }
      case 'touched': {
        const touched = strings(value)
        if (touched === null) return null
        filter.touched = touched
        break
      }
      case 'text':
        // The fallback the Brain interprets when the structured members
        // cannot express the condition; ignored beside a selector.
        break
      default:
        return null
    }
  }
  if (filter.element === undefined && filter.slot === undefined && filter.type === undefined) {
    return null
  }
  return filter
}

/** The Propositions each watched slot names, resolved before matching. */
type SlotIndex = Map<string, Set<string>>

function slotKey(slot: { subject: string; predicate: string }): string {
  return `${slot.subject} ${slot.predicate}`
}

/**
 * Reads the Propositions of one slot, so an Assertion entry — which carries
 * only `refs.proposition` — can be matched against `slot`.
 */
function slotCommand(subject: string, predicate: string): KipOperation {
  return {
    command: 'FIND(?p.id) WHERE { ?p (:subject, :predicate, ?o) } LIMIT 100',
    parameters: { subject: { id: subject }, predicate },
  }
}

/** Whether one Change Envelope entry (§36.1) is what the filter watches. */
export function entryMatches(filter: ChangeFilter, entry: unknown, slots: SlotIndex): boolean {
  if (!isObject(entry)) return false
  const text = (name: string): string | undefined =>
    typeof entry[name] === 'string' ? (entry[name] as string) : undefined
  if (filter.element !== undefined && text('id') !== filter.element) return false
  if (filter.type !== undefined) {
    // Only a Concept entry carries `schema_ref`; the type of anything else is
    // not what a `type` selector watches.
    const schemaRef = text('schema_ref')
    if (schemaRef === undefined || localName(schemaRef) !== filter.type) return false
  }
  if (filter.slot !== undefined) {
    const refs = isObject(entry.refs) ? entry.refs : {}
    const reference = (name: string): string | undefined =>
      typeof refs[name] === 'string' ? (refs[name] as string) : undefined
    let inSlot = false
    if (text('kind') === 'proposition') {
      const predicate = reference('predicate_ref')
      inSlot =
        reference('subject') === filter.slot.subject &&
        predicate !== undefined &&
        localName(predicate) === filter.slot.predicate
    } else if (text('kind') === 'assertion') {
      const proposition = reference('proposition')
      inSlot =
        proposition !== undefined &&
        (slots.get(slotKey(filter.slot))?.has(proposition) ?? false)
    }
    if (!inSlot) return false
  }
  if (filter.ops.length > 0) {
    const op = text('op')
    if (op === undefined || !filter.ops.includes(op)) return false
  }
  if (filter.touched.length > 0) {
    const touched = Array.isArray(entry.touched) ? entry.touched : []
    if (!filter.touched.some((path) => touched.includes(path))) return false
  }
  return true
}

/** One page of the Change Stream. */
function changesCommand(after: number): KipOperation {
  return {
    command: `CHANGES AFTER SEQ :after LIMIT ${CHANGES_PAGE_LIMIT}`,
    parameters: { after },
  }
}

/** The first coordinate in `envelopes` after `from` whose entries the filter matches. */
function firstMatch(
  filter: ChangeFilter,
  from: number,
  envelopes: readonly unknown[],
  slots: SlotIndex,
): number | undefined {
  for (const envelope of envelopes) {
    if (!isObject(envelope)) continue
    const seq = envelope.space_seq
    if (typeof seq !== 'number' || seq <= from) continue
    const changes = Array.isArray(envelope.changes) ? envelope.changes : []
    if (changes.some((entry) => entryMatches(filter, entry, slots))) return seq
  }
  return undefined
}

/** Why a Watch fires. */
type Fire = { kind: 'silence' } | { kind: 'delta'; seq: number }

/**
 * Fires one Watch: the transition and its provenance, atomically.
 *
 * No SleepTask, no `action_gate`, no outward act — the same three omissions the
 * Rust sweep makes, for the same reasons. The runtime knows the date passed or
 * the change landed; it does not know what that means, and recording `defer`
 * on the model's behalf would be fabricating a decision nobody made. **A fired
 * Watch grants nothing.**
 *
 * `CLIENT KEY` makes the firing idempotent under concurrent evaluators (§5.11):
 * the key names the deadline or the matched coordinate, so two sweeps that saw
 * the same event resolve to one `watch_fire` rather than writing two for it.
 * The guard names the attributes plane (§35.1) so a `MnemonicState` sweep over
 * the same Concept cannot hold a Watch armed past its deadline for a reason
 * having nothing to do with the Watch.
 */
function fireWatchCommand(
  watch: WatchRow,
  now: string,
  fire: Fire,
  evaluatedSeq?: number,
): KipOperation {
  const parameters: JsonMap = { watch: watch.id, version: watch.version, now }
  const members = ['status: "fired"', 'fired_at: :now']
  if (fire.kind === 'silence') {
    parameters.fire_key = `watch_fire:${watch.id}:silence:${watch.watch.due_at}`
  } else {
    parameters.fire_key = `watch_fire:${watch.id}:delta:${fire.seq}`
    parameters.matched_seq = fire.seq
    members.push('matched_seq: :matched_seq')
  }
  if (evaluatedSeq !== undefined) {
    parameters.evaluated_seq = evaluatedSeq
    members.push('evaluated_seq: :evaluated_seq')
  }
  return {
    command: `MUTATE {
  UPDATE :watch
    SET ATTRIBUTES { ${members.join(', ')} }
    EXPECT VERSION :version OF ATTRIBUTES

  CREATE ACTIVITY ?fire {
    CLIENT KEY :fire_key
    SET FIELDS { activity_class: "watch_fire", status: "completed" }
    SET STRUCTURAL { ("inputs", :watch) }
  }
}`,
    parameters,
  }
}

/**
 * Disarms a silence Watch whose awaited change arrived before its deadline.
 *
 * Not a fire: the Watch was waiting for *silence*, and the change is the
 * opposite of what it was armed for. The coordinate the change committed at is
 * kept on the Watch, so the reader who wonders why a silence Watch stood down
 * finds the transaction that answered it.
 */
function disarmMatchedCommand(watch: WatchRow, now: string, matchedSeq: number): KipOperation {
  return {
    command: `MUTATE {
  UPDATE :watch
    SET ATTRIBUTES { status: "disarmed", disarmed_at: :now, matched_seq: :matched_seq, evaluated_seq: :matched_seq }
    EXPECT VERSION :version OF ATTRIBUTES
}`,
    parameters: { watch: watch.id, version: watch.version, now, matched_seq: matchedSeq },
  }
}

/**
 * Records a coordinate on a Watch: `evaluated_seq` (the runtime read the
 * stream through here) or `due_seen_seq` (the head at which a sweep first saw
 * the deadline passed). The name is fixed by the caller, never by data.
 */
function stampCommand(
  watch: WatchRow,
  attribute: 'evaluated_seq' | 'due_seen_seq',
  seq: number,
): KipOperation {
  return {
    command: `MUTATE {
  UPDATE :watch
    SET ATTRIBUTES { ${attribute}: :seq }
    EXPECT VERSION :version OF ATTRIBUTES
}`,
    parameters: { watch: watch.id, version: watch.version, seq },
  }
}

/** Reads the Watches in one status, oldest deadline first. */
function watchesCommand(status: string): KipOperation {
  return {
    command:
      `FIND(${WATCH_COLUMNS}) WHERE { ` +
      '?w CONCEPT {type: "Watch"} ' +
      'FILTER(?w.attributes.status == :status) ' +
      `} ORDER BY ?w.attributes.due_at LIMIT ${WATCH_SWEEP_LIMIT}`,
    parameters: { status },
  }
}

/**
 * The silence Watches whose deadline has passed.
 *
 * `due_at` is compared as a string because graph timestamps are RFC3339 and
 * lexicographically ordered. A Watch with no `due_at` never matches, which is
 * correct: a silence Watch without a deadline has declared no moment at which
 * silence becomes meaningful.
 */
function dueSilenceWatchesCommand(now: string): KipOperation {
  return {
    command:
      `FIND(${WATCH_COLUMNS}) WHERE { ` +
      '?w CONCEPT {type: "Watch"} ' +
      'FILTER(?w.attributes.status == "armed") ' +
      'FILTER(?w.attributes.watch_class == "silence") ' +
      'FILTER(IS_NOT_NULL(?w.attributes.due_at)) ' +
      'FILTER(?w.attributes.due_at <= :now) ' +
      `} ORDER BY ?w.attributes.due_at LIMIT ${WATCH_SWEEP_LIMIT}`,
    parameters: { now },
  }
}

/** The local name of a symbol reference, or the name itself when bare. */
function localName(symbol: string): string {
  const slash = symbol.lastIndexOf('/')
  return slash === -1 ? symbol : symbol.slice(slash + 1)
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
 * An Assertion is immutable, so the cursor is the Space sequence coordinate:
 * processed revisions fall behind it, and a backlog larger than one page
 * drains across cycles. A full page never steps over its last coordinate —
 * one transaction may have superseded more claims than the page holds — so
 * the cursor stops just before it, unless the whole page shares that
 * coordinate, in which case the remainder is lost and said so.
 *
 * Reachability is topology, not judgment: a listed dependent is a candidate
 * for `DerivationState {status: "stale"}`, not already stale. The cycle decides.
 */
function scanCorrections(run: RunKip, after: number): CorrectionScan {
  const scan: CorrectionScan = { revised_roots: [], cursor: after }
  const found = run(supersededCommand(after))
  if (found.status === 'failed') {
    scan.error = found.error?.message ?? 'correction scan failed'
    return scan
  }
  const rows = Array.isArray(found.result) ? found.result : []
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
  const seqs = roots.map((root) => root.space_seq)
  if (seqs.length > 0) {
    const max = Math.max(...seqs)
    const min = Math.min(...seqs)
    if (rows.length < CORRECTION_SCAN_LIMIT) {
      scan.cursor = max
    } else if (min < max) {
      scan.cursor = max - 1
    } else {
      console.error(
        `one transaction superseded more Assertions than a settlement page holds (${CORRECTION_SCAN_LIMIT}); the remainder at ${max} will not be recorded`,
      )
      scan.cursor = max
    }
  }
  for (const root of roots) {
    if (root.space_seq > scan.cursor) continue
    const walked = run(dependentsCommand(root.assertion))
    if (walked.status === 'failed') {
      root.truncated = true
    } else {
      root.dependents = readDependents(walked.result)
      root.truncated =
        walked.next_cursor !== undefined ||
        (walked.warnings ?? []).some((warning) => isObject(warning) && warning.code === 'truncated')
    }
    scan.revised_roots.push(root)
  }
  return scan
}

function supersededCommand(after: number): KipOperation {
  return {
    command:
      'FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition, ?a.lifecycle.superseded_by) WHERE { ' +
      '?a ASSERTION {} ' +
      'FILTER(?a.lifecycle.status == "superseded") ' +
      'FILTER(?a._system.space_seq > :after) ' +
      `} ORDER BY ?a._system.space_seq LIMIT ${CORRECTION_SCAN_LIMIT}`,
    parameters: { after },
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

// --- skill lifecycle --------------------------------------------------------

/**
 * Runs the deterministic Skill lifecycle rule over graded outcomes.
 *
 * Profile §14 rule 1: promotion and demotion are executed by deterministic code
 * reading graded Outcome Evidence — "the Brain proposes, compiles, and
 * narrates; it never promotes."
 *
 * One read per Skill, because the window is per-Skill and per-cursor: a Skill
 * never graded starts from a different coordinate than one that has.
 */
function settleSkillLifecycle(run: RunKip, now: string): SkillSettlement {
  const report: SkillSettlement = { graded: 0, transitions: 0, conflicted: 0 }
  const found = run(skillsCommand())
  if (found.status === 'failed') {
    report.error = found.error?.message ?? 'skill scan failed'
    return report
  }
  for (const skill of skillRows(found.result)) {
    const graded = run(outcomesCommand(skill.id, skill.task_family, skill.cursor))
    if (graded.status === 'failed') continue
    const window = tally(graded.result)
    // Nothing attributed to this Skill since the last verdict: nothing to
    // judge, and no reason to price the family aggregate below.
    if (window.graded === 0) continue

    // The baseline a trial is measured against: the whole family up to the end
    // of this window, from which `decide` subtracts what was linked to this
    // Skill (Profile §6.5).
    const counted = run(familyTallyCommand(skill.task_family, window.cursor))
    if (counted.status === 'failed') continue

    const verdict = decide(skill, window, familyTally(counted.result))
    // An idle stream writes nothing: no verdict, no cursor move, no Activity.
    if (verdict === undefined) continue

    if (run(verdictCommand(skill, verdict, window, now)).status === 'failed') {
      // The cursor did not advance, so the next pass re-reads the same outcomes
      // and reaches the same verdict.
      report.conflicted += 1
      continue
    }
    report.graded += 1
    if (verdict.transition !== undefined) report.transitions += 1
  }
  return report
}

type SkillStatus = 'proposed' | 'trialed' | 'adopted' | 'revoked'

const SKILL_STATUSES: readonly SkillStatus[] = ['proposed', 'trialed', 'adopted', 'revoked']

interface SkillRow {
  id: string
  name: string
  version: number
  status: SkillStatus
  task_family: string
  /** Highest Outcome Evidence `space_seq` already counted into the tallies. */
  cursor: number
  /** `TrialState` (§6.5) — the basis an open trial is measured against. */
  trial: TrialBasis | undefined
  /** `GradingState` (§6.2) — linked graded outcomes only. */
  success_count: number
  failure_count: number
  graded_count: number
}

/**
 * The recorded comparison basis of an open trial (Profile §6.5).
 *
 * Stored as tallies rather than as the rate they imply: a verdict whose basis
 * was only ever a rounded number cannot be recomputed, and rule 2 asks for the
 * comparison to be recoverable rather than merely rememberable.
 */
interface TrialBasis {
  /** The `space_seq` the trial opened at. */
  basis_seq: number
  /** The family's outcomes up to `basis_seq` that were *not* linked here. */
  baseline_success: number
  baseline_failure: number
  baseline_graded: number
  /** Linked graded outcomes the rule needs before it will decide. */
  quota: number
}

/**
 * The rate the family was running at without this Skill, or `undefined` when
 * nothing in the baseline came out one way or the other.
 *
 * An empty baseline is an honest `undefined`, not a zero: a family this Skill
 * is the first to be tried in has no "how things were going", and reading that
 * as 0.0 would let any success at all clear the bar.
 */
function baselineRate(trial: TrialBasis): number | undefined {
  return successRate(trial.baseline_success, trial.baseline_failure)
}

/**
 * The whole family's graded outcomes up to a coordinate, linked or not.
 *
 * Read as one grouped aggregate rather than as a page of rows: the baseline is
 * a count over a stream that may be far larger than any window this pass would
 * read, and a truncated baseline would quietly become a comparison against the
 * most recent few runs.
 */
interface FamilyTally {
  success: number
  failure: number
  graded: number
}

function emptyFamilyTally(): FamilyTally {
  return { success: 0, failure: 0, graded: 0 }
}

/** A tally over one window of Outcome Evidence. */
interface Tally {
  success: number
  failure: number
  /** Everything the instruments graded. `unknown` is not a grade. */
  graded: number
  severeFailure: boolean
  cursor: number
  /** What the verdict Activity cites as its `inputs`. */
  evidence: string[]
}

function emptyTally(): Tally {
  return { success: 0, failure: 0, graded: 0, severeFailure: false, cursor: 0, evidence: [] }
}

/**
 * The share of decided runs that succeeded, or `undefined` when nothing was
 * decided either way. `partial` and `aborted` are graded but not decided.
 */
function successRate(success: number, failure: number): number | undefined {
  const decided = success + failure
  return decided > 0 ? success / decided : undefined
}

interface Verdict {
  /** `undefined` when the Skill stays where it is and only its tallies move. */
  transition: SkillStatus | undefined
  rationale: string
  /** The `TrialState` to write when this verdict opens (or re-opens) a trial. */
  trial: TrialBasis | undefined
}

/**
 * The deterministic rule. No model, no clock, no author assertion.
 *
 * Mirrors `anda_brain::skill::decide` decision for decision. Profile §14 rule 1:
 * "the Brain proposes, compiles, and narrates; it never promotes."
 *
 * `family` is the whole stream up to the window's end, read only to build a
 * baseline when a trial opens: the baseline is the family minus this Skill's
 * own linked outcomes (§6.5).
 */
function decide(
  skill: SkillRow,
  window: Tally,
  family: FamilyTally,
): Verdict | undefined {
  if (window.graded === 0) return undefined

  const success = skill.success_count + window.success
  const failure = skill.failure_count + window.failure
  const graded = skill.graded_count + window.graded
  const rate = successRate(success, failure)
  // Only meaningful where a trial opens; built here so every arm that opens one
  // records the same baseline for the same window.
  const opening: TrialBasis = {
    basis_seq: window.cursor,
    baseline_success: Math.max(0, family.success - success),
    baseline_failure: Math.max(0, family.failure - failure),
    baseline_graded: Math.max(0, family.graded - graded),
    quota: TRIAL_MIN_OUTCOMES,
  }
  const trial = skill.trial ?? opening
  const basis = baselineRate(trial) ?? 0
  const quota = Math.max(1, trial.quota)
  const tallyOnly = (why: string): Verdict => ({
    transition: undefined,
    rationale: `${why}: ${success} success / ${failure} failure over ${graded} linked graded under \`${skill.task_family}\``,
    trial: undefined,
  })

  switch (skill.status) {
    // A trial opens as soon as an attributed outcome arrives, and records what
    // the rest of the family was already running at — that baseline is what
    // makes the later verdict comparative rather than absolute (rule 2).
    case 'proposed': {
      const openingRate = baselineRate(opening)
      return {
        transition: 'trialed',
        rationale: `trial opened on ${window.graded} linked graded outcome(s) under \`${skill.task_family}\`; baseline ${openingRate === undefined ? 'none' : openingRate.toFixed(3)} over ${opening.baseline_graded} unlinked outcome(s)`,
        trial: opening,
      }
    }

    case 'trialed': {
      // The Profile's one sanctioned asymmetry, and it favours demotion.
      if (window.severeFailure) {
        return {
          transition: 'revoked',
          rationale: `revoked on a high-severity matching-condition failure (magnitude >= ${HIGH_SEVERITY}) under \`${skill.task_family}\``,
          trial: undefined,
        }
      }
      if (rate === undefined || graded < quota) {
        return tallyOnly('trial still gathering outcomes')
      }
      if (rate >= basis + VERDICT_MARGIN) {
        return {
          transition: 'adopted',
          rationale: `adopted: ${rate.toFixed(3)} over ${graded} linked graded outcome(s) beats the recorded baseline ${basis.toFixed(3)} by at least ${VERDICT_MARGIN}`,
          trial: undefined,
        }
      }
      if (rate <= basis - VERDICT_MARGIN) {
        return {
          transition: 'revoked',
          rationale: `revoked: ${rate.toFixed(3)} over ${graded} linked graded outcome(s) trails the recorded baseline ${basis.toFixed(3)} by at least ${VERDICT_MARGIN}`,
          trial: undefined,
        }
      }
      return tallyOnly('trial inconclusive against its baseline')
    }

    // Adoption is provisional: the stream keeps grading, and a rate that falls
    // back demotes to a new trial rather than to nothing (rule 4). The re-trial
    // writes a fresh `TrialState`, because the baseline it will be judged
    // against is the family as it stands now.
    case 'adopted': {
      if (window.severeFailure) {
        return {
          transition: 'revoked',
          rationale: `revoked: a high-severity matching-condition failure under \`${skill.task_family}\` does not wait for a re-verdict`,
          trial: undefined,
        }
      }
      if (rate !== undefined && graded >= quota && rate < basis) {
        return {
          transition: 'trialed',
          rationale: `demoted to re-trial: ${rate.toFixed(3)} has fallen back to its pre-adoption baseline ${basis.toFixed(3)}`,
          trial: opening,
        }
      }
      return tallyOnly('adoption still holding')
    }

    // Re-entry starts a new trial; nothing resurrects silently.
    case 'revoked':
      return {
        transition: 'trialed',
        rationale: `re-entry: ${window.graded} new linked graded outcome(s) under \`${skill.task_family}\` open a fresh trial`,
        trial: opening,
      }
  }
}

/** The Skills this Space holds, with the state the rule needs. */
function skillsCommand(): KipOperation {
  // Each facet is projected by name, not as the whole `facets` object: a
  // projected `?s.facets` comes back keyed by full schema ref, and a reader
  // looking up the local name would silently find nothing and grade every
  // Skill from zero.
  return {
    command:
      'FIND(?s.id, ?s.name, ?s.attributes, ?s.facets["GradingState"], ' +
      '?s.facets["TrialState"], ?s._system.plane_versions.attributes) ' +
      `WHERE { ?s CONCEPT {type: "Skill"} } LIMIT ${SKILL_SCAN_LIMIT}`,
  }
}

/**
 * Reads the Skill scan.
 *
 * A Skill without a `task_family` is skipped: the Profile requires
 * consolidation to attach one, so one that arrived anyway names no stream that
 * could grade it, and judging it on no evidence is what the lifecycle forbids.
 */
function skillRows(result: unknown): SkillRow[] {
  if (!Array.isArray(result)) return []
  const rows: SkillRow[] = []
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [id, name, attributes, grading, trialState, version] = row
    if (typeof id !== 'string' || typeof version !== 'number') continue
    const attribute = (key: string): unknown =>
      isObject(attributes) ? attributes[key] : undefined
    const tally = (key: string): number => {
      const value = isObject(grading) ? grading[key] : undefined
      return typeof value === 'number' ? value : 0
    }
    const taskFamily = attribute('task_family')
    const status = attribute('status')
    if (typeof taskFamily !== 'string' || taskFamily === '') continue
    if (typeof status !== 'string' || !SKILL_STATUSES.includes(status as SkillStatus)) continue
    const cursor = attribute(CURSOR_ATTR)
    rows.push({
      id,
      name: typeof name === 'string' ? name : '',
      version,
      status: status as SkillStatus,
      task_family: taskFamily,
      cursor: typeof cursor === 'number' ? cursor : 0,
      trial: readTrialState(trialState),
      success_count: tally('success_count'),
      failure_count: tally('failure_count'),
      graded_count: tally('graded_count'),
    })
  }
  return rows
}

/**
 * Reads a `TrialState` facet into the basis a verdict compares against.
 *
 * `basis_seq` is the one member the Profile makes required, so a facet without
 * it is a trial nobody opened and answers `undefined` — the next verdict then
 * opens one properly instead of judging against zeroes it invented.
 */
function readTrialState(facet: unknown): TrialBasis | undefined {
  if (!isObject(facet)) return undefined
  const count = (key: string): number => {
    const value = facet[key]
    return typeof value === 'number' ? value : 0
  }
  if (typeof facet.basis_seq !== 'number') return undefined
  const quota = facet.quota
  return {
    basis_seq: facet.basis_seq,
    baseline_success: count('baseline_success_count'),
    baseline_failure: count('baseline_failure_count'),
    baseline_graded: count('baseline_graded_count'),
    quota: typeof quota === 'number' && quota > 0 ? quota : TRIAL_MIN_OUTCOMES,
  }
}

/**
 * The Outcome Evidence **linked to a decision that applied this Skill** and not
 * yet counted — the treatment set (Profile §8.1, §14 rule 7).
 *
 * The two hops are the attribution, and neither is optional. An instrument
 * writes an `outcome_observation` Activity naming the `action_gate` decision it
 * observed among its `inputs` and the Outcome Evidence among its `outputs`; the
 * gate names the Skill it applied among its own `inputs`. Joining on
 * `task_family` alone would let one Skill be promoted by another Skill's runs,
 * which is the failure rule 7 exists to name.
 *
 * The cursor is a Space sequence coordinate: without it a replayed pass would
 * count the same run twice and promote on arithmetic rather than evidence.
 */
function outcomesCommand(
  skill: string,
  taskFamily: string,
  after: number,
): KipOperation {
  return {
    command:
      'FIND(?e.id, ?e._system.space_seq, ?e.facets["OutcomeRecord"]) WHERE { ' +
      '?gate ACTIVITY {activity_class: "action_gate"} ' +
      'STRUCTURAL (?gate, "inputs", :skill) ' +
      '?obs ACTIVITY {activity_class: "outcome_observation"} ' +
      'STRUCTURAL (?obs, "inputs", ?gate) ' +
      '?e EVIDENCE {evidence_class: "outcome"} ' +
      'STRUCTURAL (?obs, "outputs", ?e) ' +
      'FILTER(?e.facets["OutcomeRecord"].task_family == :family) ' +
      'FILTER(?e._system.space_seq > :after) ' +
      `} ORDER BY ?e._system.space_seq LIMIT ${OUTCOME_WINDOW}`,
    parameters: { skill, family: taskFamily, after },
  }
}

/**
 * The whole family's graded outcomes up to a coordinate — the stream a trial's
 * baseline is drawn from (Profile §6.5).
 *
 * One grouped aggregate (§44.6) rather than a page of rows. It counts the
 * linked outcomes too; the caller subtracts this Skill's own, which is cheaper
 * and more exact than asking the engine for a negation.
 */
function familyTallyCommand(taskFamily: string, upto: number): KipOperation {
  return {
    command:
      'FIND(?e.facets["OutcomeRecord"].outcome_status, COUNT(?e)) WHERE { ' +
      '?e EVIDENCE {evidence_class: "outcome"} ' +
      'FILTER(?e.facets["OutcomeRecord"].task_family == :family) ' +
      'FILTER(?e._system.space_seq <= :upto) ' +
      '}',
    parameters: { family: taskFamily, upto },
  }
}

/**
 * Reads the grouped family aggregate.
 *
 * `unknown` is not a grade, so it is counted nowhere — the same rule the window
 * tally applies, because a baseline graded on a different vocabulary than the
 * treatment set is not a comparison.
 */
function familyTally(result: unknown): FamilyTally {
  const family = emptyFamilyTally()
  if (!Array.isArray(result)) return family
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [status, count] = row
    if (typeof count !== 'number') continue
    switch (status) {
      case 'success':
        family.success += count
        family.graded += count
        break
      case 'failure':
        family.failure += count
        family.graded += count
        break
      case 'partial':
      case 'aborted':
        family.graded += count
        break
      default:
        break
    }
  }
  return family
}

/**
 * Grades one window of Outcome Evidence.
 *
 * `aborted` deliberately does not count against the Skill: rule 5 says a
 * failure under non-matching conditions narrows applicability without
 * penalizing the procedure. A deploy that never started is not evidence the
 * recipe is wrong.
 */
function tally(result: unknown): Tally {
  const graded = emptyTally()
  if (!Array.isArray(result)) return graded
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [id, seq, record] = row
    if (typeof id !== 'string' || typeof seq !== 'number') continue
    const status = isObject(record) ? record.outcome_status : undefined
    const magnitude = isObject(record) ? record.magnitude : undefined

    graded.cursor = Math.max(graded.cursor, seq)
    if (status !== 'unknown' && typeof status === 'string') graded.evidence.push(id)
    switch (status) {
      case 'success':
        graded.success += 1
        graded.graded += 1
        break
      case 'failure':
        graded.failure += 1
        graded.graded += 1
        if (typeof magnitude === 'number' && magnitude >= HIGH_SEVERITY) {
          graded.severeFailure = true
        }
        break
      case 'partial':
      case 'aborted':
        graded.graded += 1
        break
      default:
        // `unknown` is not a grade at all.
        break
    }
  }
  return graded
}

/**
 * Writes one verdict: the guarded transition and its provenance, atomically.
 *
 * `inputs` is the linked Outcome Evidence and `outputs` is the Skill it moved
 * (Profile §9) — the other way round would read as though the Skill caused the
 * runs that graded it. `parameters_digest` pins rule identity and the window,
 * because `Activity` is a Core kind whose `SET FIELDS` takes only Core fields
 * and because that is where the Profile says to put them; the basis lives on
 * the Skill's own `TrialState`.
 *
 * Three facets, three jobs (§6.1, §6.2, §6.5): `GradingState` takes the tallies
 * of what happened, `MnemonicState.utility` takes the revised bet on what will,
 * and `TrialState` — only when this verdict opens a trial — takes what the next
 * one will measure against. The 2.0 draft carried all three in one
 * `SkillUtility` facet; splitting them is what stops a tally from reading as a
 * forecast.
 */
function verdictCommand(
  skill: SkillRow,
  verdict: Verdict,
  window: Tally,
  now: string,
): KipOperation {
  const status = verdict.transition ?? skill.status
  const success = skill.success_count + window.success
  const failure = skill.failure_count + window.failure
  const graded = skill.graded_count + window.graded
  // The revised admission bet: the observed share of decided linked runs that
  // worked. Procedural standing, never truth and never permission.
  const utility = successRate(success, failure) ?? 0

  const basis = verdict.trial ?? skill.trial
  const basisRate = basis === undefined ? undefined : baselineRate(basis)
  const digest =
    `rule=${VERDICT_RULE} family=${skill.task_family} ` +
    `basis_seq=${basis === undefined ? 'none' : basis.basis_seq} ` +
    `baseline=${basisRate === undefined ? 'none' : basisRate.toFixed(3)} ` +
    `window=(${skill.cursor},${window.cursor}] ` +
    `tally=${success}s/${failure}f/${graded}g verdict=${skill.status}->${status}`

  const parameters: JsonMap = {
    skill: skill.id,
    version: skill.version,
    status,
    cursor: window.cursor,
    utility,
    success,
    failure,
    graded,
    now,
    digest,
  }
  // `TrialState` is rewritten only by a verdict that opens a trial (§6.5); a
  // verdict that decides one leaves the basis it was decided against standing,
  // so the decision stays checkable after the fact.
  let trialState = ''
  if (verdict.trial !== undefined) {
    parameters.basis_seq = verdict.trial.basis_seq
    parameters.b_success = verdict.trial.baseline_success
    parameters.b_failure = verdict.trial.baseline_failure
    parameters.b_graded = verdict.trial.baseline_graded
    parameters.quota = verdict.trial.quota
    parameters.rule = VERDICT_RULE
    trialState = `
    SET FACET "TrialState" {
      opened_at: :now,
      basis_seq: :basis_seq,
      baseline_success_count: :b_success,
      baseline_failure_count: :b_failure,
      baseline_graded_count: :b_graded,
      quota: :quota,
      rule_id: :rule
    }`
  }

  const inputs = window.evidence
    .map((id, index) => {
      parameters[`e${index}`] = id
      return `("inputs", :e${index}) `
    })
    .join('')

  return {
    command: `MUTATE {
  UPDATE :skill
    SET ATTRIBUTES { status: :status, ${CURSOR_ATTR}: :cursor }
    SET FACET "GradingState" {
      success_count: :success,
      failure_count: :failure,
      graded_count: :graded,
      last_verdict_at: :now
    }
    SET FACET "MnemonicState" { utility: :utility }${trialState}
    EXPECT VERSION :version OF ATTRIBUTES

  CREATE ACTIVITY ?verdict {
    SET FIELDS {
      activity_class: "lifecycle_verdict",
      status: "completed",
      parameters_digest: :digest
    }
    SET STRUCTURAL { ${inputs}("outputs", :skill) }
  }
}`,
    parameters,
  }
}

// --- mnemonic metabolism ----------------------------------------------------

/**
 * Disuse metabolism: `MnemonicState.memory_strength`, never confidence.
 *
 * The `last_metabolized_at` filter is both the weekly rate limit and the
 * intra-sweep cursor, so a Space with nothing due costs one query that matches
 * no rows. A failed sweep decays nothing and says so by reporting zero — the
 * cycle still runs.
 */
function metabolize(run: RunKip, now: string, metabolizedBefore: string): number {
  const result = run(decayCommand(now, metabolizedBefore))
  if (result.status === 'failed') return 0
  const changes = result.extensions?.['kip-do/outcome']?.changes ?? []
  return changes.filter((change) => change.op === 'update').length
}

/**
 * One disuse-metabolism batch.
 *
 * Decays `MnemonicState.memory_strength` — accessibility — and never Assertion
 * confidence: a fact nobody has asked about in a month is no less credible, and
 * KIP 2.0 forbids letting time erode a stance.
 *
 * A Concept that has never carried the Facet is metabolized from a baseline
 * rather than skipped: leaving it out would make "the model forgot to set
 * MnemonicState" mean "this memory never fades", which is not a decision
 * anybody made. The `last_metabolized_at` filter is both the weekly rate limit
 * and the intra-sweep cursor — rows stamped by this pass stop matching.
 */
function decayCommand(now: string, metabolizedBefore: string): KipOperation {
  return {
    command: `UPDATE ?c
SET FACET "MnemonicState" {
  memory_strength: CLAMP(MUL(COALESCE(?c.facets["MnemonicState"].memory_strength, :baseline), :factor), :floor, 1.0),
  last_metabolized_at: :now
}
WHERE {
  ?c CONCEPT {}
  FILTER(IS_NULL(?c.facets["MnemonicState"].last_metabolized_at) || ?c.facets["MnemonicState"].last_metabolized_at < :before)
  FILTER(IS_NULL(?c.facets["MnemonicState"].memory_strength) || ?c.facets["MnemonicState"].memory_strength > :floor)
}
LIMIT :limit`,
    parameters: {
      baseline: DEFAULT_MEMORY_STRENGTH,
      factor: DECAY_FACTOR,
      floor: DECAY_FLOOR,
      now,
      before: metabolizedBefore,
      limit: DECAY_BATCH_LIMIT,
    },
  }
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
  const census: Record<string, number> = {}
  const listed = run({ command: 'LIST PREDICATES LIMIT 100' })
  if (listed.status === 'failed' || !Array.isArray(listed.result)) return census
  for (const entry of listed.result) {
    // A `LIST` row, the same one both engines answer with:
    // `{ref, local_name, package_ref, status}`. `local_name` is what a command
    // may write, which is what the census counts by.
    if (!isObject(entry)) continue
    const name = entry.local_name
    if (typeof name !== 'string' || name === '') continue
    const counted = run(predicateCensusCommand(name))
    if (counted.status === 'failed') continue
    const count = Array.isArray(counted.result) ? counted.result[0] : undefined
    if (typeof count === 'number') census[name] = count
  }
  return census
}

/** How many links each registered predicate carries — the sprawl indicator. */
function predicateCensusCommand(predicate: string): KipOperation {
  return {
    command: 'FIND(COUNT(?link)) WHERE { ?link (?s, :predicate, ?o) }',
    parameters: { predicate },
  }
}

// --- helpers ----------------------------------------------------------------

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
