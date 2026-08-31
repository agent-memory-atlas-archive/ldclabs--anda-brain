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
import type { ArmedWatch, SettlementReport, SkillSettlement, WatchSettlement } from './types.js'

// --- rule constants, mirrored from anda_brain -------------------------------

/**
 * The identity of the Skill verdict rule, recorded on every verdict.
 *
 * Identical to `anda_brain::skill::VERDICT_RULE` on purpose: an auditor
 * recomputing a verdict must get the same answer whichever deployment wrote it.
 * Bump both together, never one.
 */
export const VERDICT_RULE = 'anda-brain/skill-verdict@1'

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

/** Deployment-local Skill attributes; see `anda_brain::skill`. */
const CURSOR_ATTR = 'verdict_cursor'
const BASIS_ATTR = 'trial_basis'
const BASIS_N_ATTR = 'trial_basis_n'

// --- watch expiry -----------------------------------------------------------

/**
 * The silence Watches whose deadline has passed.
 *
 * A `delta` Watch is the model's to evaluate: its condition is prose the
 * Profile deliberately fixes no language for. A `silence` Watch at its
 * deadline is arithmetic — one still `armed` is one no evaluation has fired.
 */
export function dueSilenceWatchesCommand(now: string): KipOperation {
  return {
    command:
      'FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version) WHERE { ' +
      '?w CONCEPT {type: "Watch"} ' +
      'FILTER(?w.attributes.status == "armed") ' +
      'FILTER(?w.attributes.watch_class == "silence") ' +
      'FILTER(IS_NOT_NULL(?w.attributes.due_at)) ' +
      'FILTER(?w.attributes.due_at <= :now) ' +
      `} ORDER BY ?w.attributes.due_at LIMIT ${WATCH_SWEEP_LIMIT}`,
    parameters: { now },
  }
}

/** One due Watch, with what the guarded update needs. */
export interface DueWatch {
  id: string
  version: number
  watch: ArmedWatch
}

/**
 * Reads the scan rows.
 *
 * A row missing its id or version is skipped rather than fired: firing without
 * the version would overwrite a concurrent edit instead of yielding to it.
 */
export function dueWatches(result: unknown): DueWatch[] {
  if (!Array.isArray(result)) return []
  const due: DueWatch[] = []
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [id, name, attributes, version] = row
    if (typeof id !== 'string' || typeof version !== 'number') continue
    due.push({
      id,
      version,
      watch: readWatch(id, name, attributes),
    })
  }
  return due
}

function readWatch(id: string, name: unknown, attributes: unknown): ArmedWatch {
  const attribute = (key: string): string => {
    const value = isObject(attributes) ? attributes[key] : undefined
    return typeof value === 'string' ? value : ''
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
 * Fires one silence Watch: the transition and its provenance, atomically.
 *
 * No SleepTask, no `action_gate`, no outward act — the same three omissions the
 * Rust sweep makes, for the same reasons. The runtime knows the date passed; it
 * does not know what that means, and recording `defer` on the model's behalf
 * would be fabricating a decision nobody made. **A fired Watch grants nothing.**
 */
export function fireWatchCommand(watch: DueWatch, now: string): KipOperation {
  return {
    command: `MUTATE {
  UPDATE :watch
    EXPECT VERSION :version
    SET ATTRIBUTES { status: "fired", fired_at: :now }

  CREATE ACTIVITY ?fire {
    SET FIELDS { activity_class: "watch_fire", status: "completed" }
    SET STRUCTURAL { ("inputs", :watch) }
  }
}`,
    parameters: { watch: watch.id, version: watch.version, now },
  }
}

// --- skill lifecycle --------------------------------------------------------

export type SkillStatus = 'proposed' | 'trialed' | 'adopted' | 'revoked'

const SKILL_STATUSES: readonly SkillStatus[] = ['proposed', 'trialed', 'adopted', 'revoked']

export interface SkillRow {
  id: string
  name: string
  version: number
  status: SkillStatus
  task_family: string
  cursor: number
  basis: number | undefined
  success_count: number
  failure_count: number
  graded_count: number
}

/** A tally over one window of Outcome Evidence. */
export interface Tally {
  success: number
  failure: number
  /** Everything the instruments graded. `unknown` is not a grade. */
  graded: number
  severeFailure: boolean
  cursor: number
  /** What the verdict Activity cites as its `inputs`. */
  evidence: string[]
}

export function emptyTally(): Tally {
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

export interface Verdict {
  /** `undefined` when the Skill stays where it is and only its tallies move. */
  transition: SkillStatus | undefined
  rationale: string
  basis: number | undefined
}

/**
 * The deterministic rule. No model, no clock, no author assertion.
 *
 * Mirrors `anda_brain::skill::decide` decision for decision. Profile §14 rule 1:
 * "the Brain proposes, compiles, and narrates; it never promotes."
 */
export function decide(skill: SkillRow, window: Tally): Verdict | undefined {
  if (window.graded === 0) return undefined

  const success = skill.success_count + window.success
  const failure = skill.failure_count + window.failure
  const graded = skill.graded_count + window.graded
  const rate = successRate(success, failure)
  const basis = skill.basis ?? 0
  const tallyOnly = (why: string): Verdict => ({
    transition: undefined,
    rationale: `${why}: ${success} success / ${failure} failure over ${graded} graded under \`${skill.task_family}\``,
    basis: undefined,
  })

  switch (skill.status) {
    // A trial opens as soon as the stream produces anything, and records what
    // the family was already running at — that basis is what makes the later
    // verdict comparative rather than absolute (rule 2).
    case 'proposed':
      return {
        transition: 'trialed',
        rationale: `trial opened on ${window.graded} graded outcome(s) under \`${skill.task_family}\`; basis ${rate === undefined ? 'none' : rate.toFixed(3)}`,
        basis: rate,
      }

    case 'trialed': {
      // The Profile's one sanctioned asymmetry, and it favours demotion.
      if (window.severeFailure) {
        return {
          transition: 'revoked',
          rationale: `revoked on a high-severity matching-condition failure (magnitude >= ${HIGH_SEVERITY}) under \`${skill.task_family}\``,
          basis: undefined,
        }
      }
      if (rate === undefined || graded < TRIAL_MIN_OUTCOMES) {
        return tallyOnly('trial still gathering outcomes')
      }
      if (rate >= basis + VERDICT_MARGIN) {
        return {
          transition: 'adopted',
          rationale: `adopted: ${rate.toFixed(3)} over ${graded} graded outcome(s) beats the recorded basis ${basis.toFixed(3)} by at least ${VERDICT_MARGIN}`,
          basis: undefined,
        }
      }
      if (rate <= basis - VERDICT_MARGIN) {
        return {
          transition: 'revoked',
          rationale: `revoked: ${rate.toFixed(3)} over ${graded} graded outcome(s) trails the recorded basis ${basis.toFixed(3)} by at least ${VERDICT_MARGIN}`,
          basis: undefined,
        }
      }
      return tallyOnly('trial inconclusive against its basis')
    }

    // Adoption is provisional: the stream keeps grading, and a rate that falls
    // back demotes to a new trial rather than to nothing (rule 4).
    case 'adopted': {
      if (window.severeFailure) {
        return {
          transition: 'revoked',
          rationale: `revoked: a high-severity matching-condition failure under \`${skill.task_family}\` does not wait for a re-verdict`,
          basis: undefined,
        }
      }
      if (rate !== undefined && graded >= TRIAL_MIN_OUTCOMES && rate < basis) {
        return {
          transition: 'trialed',
          rationale: `demoted to re-trial: ${rate.toFixed(3)} has fallen back to its pre-adoption basis ${basis.toFixed(3)}`,
          basis: rate,
        }
      }
      return tallyOnly('adoption still holding')
    }

    // Re-entry starts a new trial; nothing resurrects silently.
    case 'revoked':
      return {
        transition: 'trialed',
        rationale: `re-entry: ${window.graded} new graded outcome(s) under \`${skill.task_family}\` open a fresh trial`,
        basis: rate,
      }
  }
}

/** The Skills this Space holds, with the state the rule needs. */
export function skillsCommand(): KipOperation {
  // The facet is projected by name, not as the whole `facets` object: a
  // projected `?s.facets` comes back keyed by full schema ref, and a reader
  // looking up the local name would silently find nothing and grade every
  // Skill from zero.
  return {
    command:
      'FIND(?s.id, ?s.name, ?s.attributes, ?s.facets["SkillUtility"], ?s._system.version) ' +
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
export function skillRows(result: unknown): SkillRow[] {
  if (!Array.isArray(result)) return []
  const rows: SkillRow[] = []
  for (const row of result) {
    if (!Array.isArray(row)) continue
    const [id, name, attributes, utility, version] = row
    if (typeof id !== 'string' || typeof version !== 'number') continue
    const attribute = (key: string): unknown =>
      isObject(attributes) ? attributes[key] : undefined
    const tally = (key: string): number => {
      const value = isObject(utility) ? utility[key] : undefined
      return typeof value === 'number' ? value : 0
    }
    const taskFamily = attribute('task_family')
    const status = attribute('status')
    if (typeof taskFamily !== 'string' || taskFamily === '') continue
    if (typeof status !== 'string' || !SKILL_STATUSES.includes(status as SkillStatus)) continue
    const cursor = attribute(CURSOR_ATTR)
    const basis = attribute(BASIS_ATTR)
    rows.push({
      id,
      name: typeof name === 'string' ? name : '',
      version,
      status: status as SkillStatus,
      task_family: taskFamily,
      cursor: typeof cursor === 'number' ? cursor : 0,
      basis: typeof basis === 'number' ? basis : undefined,
      success_count: tally('success_count'),
      failure_count: tally('failure_count'),
      graded_count: tally('graded_count'),
    })
  }
  return rows
}

/**
 * The Outcome Evidence for one task family that this Skill has not counted.
 *
 * The cursor is a Space sequence coordinate: without it a replayed pass would
 * count the same run twice and promote on arithmetic rather than evidence.
 */
export function outcomesCommand(taskFamily: string, after: number): KipOperation {
  return {
    command:
      'FIND(?e.id, ?e._system.space_seq, ?e.facets["OutcomeRecord"]) WHERE { ' +
      '?e EVIDENCE {evidence_class: "outcome"} ' +
      'FILTER(?e.facets["OutcomeRecord"].task_family == :family) ' +
      'FILTER(?e._system.space_seq > :after) ' +
      `} ORDER BY ?e._system.space_seq LIMIT ${OUTCOME_WINDOW}`,
    parameters: { family: taskFamily, after },
  }
}

/**
 * Grades one window of Outcome Evidence.
 *
 * `aborted` deliberately does not count against the Skill: rule 5 says a
 * failure under non-matching conditions narrows applicability without
 * penalizing the procedure. A deploy that never started is not evidence the
 * recipe is wrong.
 */
export function tally(result: unknown): Tally {
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
 * `inputs` is the graded Outcome Evidence and `outputs` is the Skill it moved
 * (Profile §12) — the other way round would read as though the Skill caused the
 * runs that graded it. `parameters_digest` pins rule identity and comparison
 * basis, because `Activity` is a Core kind whose `SET FIELDS` takes only Core
 * fields and because that is where the Profile says to put them.
 */
export function verdictCommand(
  skill: SkillRow,
  verdict: Verdict,
  window: Tally,
  now: string,
): KipOperation {
  const status = verdict.transition ?? skill.status
  const success = skill.success_count + window.success
  const failure = skill.failure_count + window.failure
  const graded = skill.graded_count + window.graded
  // Procedural standing, never truth and never permission.
  const utility = successRate(success, failure) ?? 0

  const basisForDigest = verdict.basis ?? skill.basis
  const digest =
    `rule=${VERDICT_RULE} family=${skill.task_family} ` +
    `basis=${basisForDigest === undefined ? 'none' : basisForDigest.toFixed(3)} ` +
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
  let basisAssignment = ''
  if (verdict.basis !== undefined) {
    parameters.basis = verdict.basis
    parameters.basis_n = window.graded
    basisAssignment = `, ${BASIS_ATTR}: :basis, ${BASIS_N_ATTR}: :basis_n`
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
    EXPECT VERSION :version
    SET ATTRIBUTES { status: :status, ${CURSOR_ATTR}: :cursor${basisAssignment} }
    SET FACET "SkillUtility" {
      utility: :utility,
      success_count: :success,
      failure_count: :failure,
      graded_count: :graded,
      last_verdict_at: :now
    }

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
export function decayCommand(now: string, metabolizedBefore: string): KipOperation {
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

export function decayHorizon(nowMs: number): number {
  return nowMs - DECAY_MIN_INTERVAL_MS
}

// --- assessment reads -------------------------------------------------------

/** The Watches this Space holds in one status. */
export function watchesCommand(status: string): KipOperation {
  return {
    command:
      'FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version) WHERE { ' +
      '?w CONCEPT {type: "Watch"} ' +
      'FILTER(?w.attributes.status == :status) ' +
      `} ORDER BY ?w.attributes.due_at LIMIT ${WATCH_SWEEP_LIMIT}`,
    parameters: { status },
  }
}

export function readWatches(result: unknown): ArmedWatch[] {
  return dueWatches(result).map((due) => due.watch)
}

/** How many links each registered predicate carries — the sprawl indicator. */
export function predicateCensusCommand(predicate: string): KipOperation {
  return {
    command: 'FIND(COUNT(?link)) WHERE { ?link (?s, :predicate, ?o) }',
    parameters: { predicate },
  }
}

// --- helpers ----------------------------------------------------------------

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/** Whether a write failed because the element moved under the sweep. */
export function isVersionConflict(result: KipResult): boolean {
  const code = result.error?.code ?? ''
  return code.includes('Version') || code.includes('Precondition')
}

export function emptyWatchSettlement(): WatchSettlement {
  return { fired: 0, conflicted: 0 }
}

export function emptySkillSettlement(): SkillSettlement {
  return { graded: 0, transitions: 0, conflicted: 0 }
}

export function emptySettlement(settledAt: string): SettlementReport {
  return {
    settled_at: settledAt,
    decayed: 0,
    watches: emptyWatchSettlement(),
    skills: emptySkillSettlement(),
  }
}
