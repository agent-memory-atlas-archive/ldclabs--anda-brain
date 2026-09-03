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
      'FIND(?w.id, ?w.name, ?w.attributes, ?w._system.plane_versions.attributes) WHERE { ' +
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
 * Fires one silence Watch: the transition and its provenance, atomically.
 *
 * No SleepTask, no `action_gate`, no outward act — the same three omissions the
 * Rust sweep makes, for the same reasons. The runtime knows the date passed; it
 * does not know what that means, and recording `defer` on the model's behalf
 * would be fabricating a decision nobody made. **A fired Watch grants nothing.**
 *
 * `CLIENT KEY` makes the firing idempotent under concurrent evaluators (§5.11):
 * two sweeps that saw the same passed deadline resolve to one `watch_fire`
 * rather than writing two for one silence. The guard names the attributes plane
 * (§35.1) so a `MnemonicState` sweep over the same Concept cannot hold a Watch
 * armed past its deadline for a reason having nothing to do with the Watch.
 */
export function fireWatchCommand(watch: DueWatch, now: string): KipOperation {
  return {
    command: `MUTATE {
  UPDATE :watch
    SET ATTRIBUTES { status: "fired", fired_at: :now }
    EXPECT VERSION :version OF ATTRIBUTES

  CREATE ACTIVITY ?fire {
    CLIENT KEY :fire_key
    SET FIELDS { activity_class: "watch_fire", status: "completed" }
    SET STRUCTURAL { ("inputs", :watch) }
  }
}`,
    parameters: {
      watch: watch.id,
      version: watch.version,
      now,
      fire_key: `watch_fire:${watch.id}:silence:${watch.watch.due_at}`,
    },
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
export interface TrialBasis {
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
export function baselineRate(trial: TrialBasis): number | undefined {
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
export interface FamilyTally {
  success: number
  failure: number
  graded: number
}

export function emptyFamilyTally(): FamilyTally {
  return { success: 0, failure: 0, graded: 0 }
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
export function decide(
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
export function skillsCommand(): KipOperation {
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
export function skillRows(result: unknown): SkillRow[] {
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
export function outcomesCommand(
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
export function familyTallyCommand(taskFamily: string, upto: number): KipOperation {
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
export function familyTally(result: unknown): FamilyTally {
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
      'FIND(?w.id, ?w.name, ?w.attributes, ?w._system.plane_versions.attributes) WHERE { ' +
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
