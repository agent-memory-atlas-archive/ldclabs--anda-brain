/**
 * The deterministic settlement, driven through its own port.
 *
 * `settle` takes a `RunKip` rather than reaching for a Durable Object, so the
 * verdict rule can be exercised across every arm without building five graphs
 * to produce five outcome streams. The HTTP suite still covers one path against
 * a real nexus; what is here is the rule table, which is the half that has to
 * agree with `anda_brain/src/skill.rs` decision for decision.
 */

import type { KipResult } from '@ldclabs/kip-do'
import { describe, expect, it } from 'vitest'
import type { KipOperation } from '../src/kip.js'
import { settle } from '../src/settle.js'

const NOW = Date.parse('2026-09-03T00:00:00Z')

const OK: KipResult = { status: 'succeeded' }

function rows(result: unknown[]): KipResult {
  return { status: 'succeeded', result: result as never }
}

/** A Skill scan row: `(id, name, attributes, GradingState, TrialState, version)`. */
function skill(options: {
  status: string
  cursor?: number
  grading?: Record<string, number>
  trial?: Record<string, number | string>
}): unknown[] {
  return [
    'C-1',
    'Rollback first',
    {
      status: options.status,
      task_family: 'deploy',
      ...(options.cursor === undefined ? {} : { verdict_cursor: options.cursor }),
    },
    options.grading ?? null,
    options.trial ?? null,
    3,
  ]
}

/** An Outcome Evidence row: `(id, space_seq, OutcomeRecord)`. */
function outcome(seq: number, outcome_status: string, magnitude?: number): unknown[] {
  return [
    `E-${seq}`,
    seq,
    { task_family: 'deploy', outcome_status, ...(magnitude === undefined ? {} : { magnitude }) },
  ]
}

interface Graph {
  skills?: unknown[][]
  outcomes?: unknown[][]
  /** Grouped `(outcome_status, count)` rows for the whole task family. */
  family?: unknown[][]
  watches?: unknown[][]
}

/**
 * A `RunKip` that answers from a fixture and records what it was asked to write.
 *
 * Dispatch is on the command text because that is what the port carries; each
 * marker below is a fragment only one of the settlement's commands contains.
 */
function fakeRun(graph: Graph): { run: (op: KipOperation) => KipResult; writes: KipOperation[] } {
  const writes: KipOperation[] = []
  const run = (operation: KipOperation): KipResult => {
    const { command } = operation
    if (command.startsWith('UPDATE ?c')) return OK
    if (command.includes('MUTATE')) {
      writes.push(operation)
      return OK
    }
    if (command.includes('type: "Watch"')) return rows(graph.watches ?? [])
    if (command.includes('type: "Skill"')) return rows(graph.skills ?? [])
    if (command.includes('COUNT(?e)')) return rows(graph.family ?? [])
    if (command.includes('evidence_class: "outcome"')) return rows(graph.outcomes ?? [])
    throw new Error(`unscripted command: ${command}`)
  }
  return { run, writes }
}

/** The verdict this settlement wrote, read off the guarded update's bindings. */
function verdict(writes: KipOperation[]): Record<string, unknown> {
  expect(writes).toHaveLength(1)
  return (writes[0]?.parameters ?? {}) as Record<string, unknown>
}

/** `n` graded outcomes, `successes` of them successful, starting at seq 101. */
function stream(successes: number, failures: number): unknown[][] {
  const records: unknown[][] = []
  let seq = 101
  for (let index = 0; index < successes; index += 1) records.push(outcome(seq++, 'success'))
  for (let index = 0; index < failures; index += 1) records.push(outcome(seq++, 'failure'))
  return records
}

/** A family baseline running at `successes / (successes + failures)`. */
function family(successes: number, failures: number): unknown[][] {
  return [
    ['success', successes],
    ['failure', failures],
  ]
}

describe('deterministic settlement', () => {
  it('opens a trial on the first attributed outcome and records its baseline', () => {
    const { run, writes } = fakeRun({
      skills: [skill({ status: 'proposed' })],
      outcomes: stream(1, 0),
      // The family had 8 successes and 2 failures; one success is this Skill's.
      family: family(8, 2),
    })

    const report = settle(run, NOW)

    expect(report.skills).toMatchObject({ graded: 1, transitions: 1, conflicted: 0 })
    const written = verdict(writes)
    expect(written.status).toBe('trialed')
    // The baseline is the family minus this Skill's own linked outcomes:
    // 7 successes and 2 failures, not 8 and 2.
    expect(written.b_success).toBe(7)
    expect(written.b_failure).toBe(2)
    expect(written.cursor).toBe(101)
  })

  it('adopts a trial that beats its recorded baseline by the margin', () => {
    const { run, writes } = fakeRun({
      skills: [
        skill({
          status: 'trialed',
          cursor: 100,
          // Baseline 0.500; the quota is 5 linked graded outcomes.
          trial: { basis_seq: 100, baseline_success_count: 5, baseline_failure_count: 5 },
        }),
      ],
      outcomes: stream(6, 0),
      family: family(11, 5),
    })

    const report = settle(run, NOW)

    expect(report.skills.transitions).toBe(1)
    const written = verdict(writes)
    expect(written.status).toBe('adopted')
    expect(written.utility).toBe(1)
    expect(String(written.digest)).toContain('verdict=trialed->adopted')
  })

  it('revokes a trial that trails its baseline by the margin', () => {
    const { run, writes } = fakeRun({
      skills: [
        skill({
          status: 'trialed',
          cursor: 100,
          trial: { basis_seq: 100, baseline_success_count: 8, baseline_failure_count: 2 },
        }),
      ],
      outcomes: stream(2, 4),
      family: family(10, 6),
    })

    settle(run, NOW)

    expect(verdict(writes).status).toBe('revoked')
  })

  it('holds a trial that has not met its quota, moving the tallies only', () => {
    const { run, writes } = fakeRun({
      skills: [
        skill({
          status: 'trialed',
          cursor: 100,
          trial: { basis_seq: 100, baseline_success_count: 5, baseline_failure_count: 5 },
        }),
      ],
      outcomes: stream(2, 0),
      family: family(7, 5),
    })

    const report = settle(run, NOW)

    expect(report.skills).toMatchObject({ graded: 1, transitions: 0 })
    // Still `trialed`: a verdict wrote the tallies, not a transition.
    expect(verdict(writes).status).toBe('trialed')
  })

  it('revokes on one high-severity failure without waiting for the quota', () => {
    const { run, writes } = fakeRun({
      skills: [
        skill({
          status: 'adopted',
          cursor: 100,
          trial: { basis_seq: 100, baseline_success_count: 5, baseline_failure_count: 5 },
        }),
      ],
      outcomes: [outcome(101, 'failure', 0.9)],
      family: family(5, 6),
    })

    settle(run, NOW)

    const written = verdict(writes)
    expect(written.status).toBe('revoked')
    expect(String(written.digest)).toContain('verdict=adopted->revoked')
  })

  it('re-opens a trial for a revoked Skill rather than resurrecting it', () => {
    const { run, writes } = fakeRun({
      skills: [skill({ status: 'revoked', cursor: 100 })],
      outcomes: stream(1, 0),
      family: family(4, 1),
    })

    settle(run, NOW)

    expect(verdict(writes).status).toBe('trialed')
  })

  it('judges nothing when no outcome is attributed to the Skill', () => {
    const { run, writes } = fakeRun({
      skills: [skill({ status: 'trialed', cursor: 100 })],
      outcomes: [],
    })

    const report = settle(run, NOW)

    expect(report.skills).toEqual({ graded: 0, transitions: 0, conflicted: 0 })
    expect(writes).toHaveLength(0)
  })

  it('counts a refused verdict as a conflict and leaves the cursor alone', () => {
    const base = fakeRun({
      skills: [skill({ status: 'proposed' })],
      outcomes: stream(1, 0),
      family: family(1, 0),
    })
    const run = (operation: KipOperation): KipResult =>
      operation.command.includes('UPDATE :skill')
        ? { status: 'failed', error: { code: 'PreconditionFailed', message: 'moved' } as never }
        : base.run(operation)

    const report = settle(run, NOW)

    expect(report.skills).toMatchObject({ graded: 0, transitions: 0, conflicted: 1 })
  })

  it('reports a failed scan as a degraded pass, not a failed cycle', () => {
    const run = (operation: KipOperation): KipResult =>
      operation.command.startsWith('UPDATE ?c')
        ? { status: 'succeeded' }
        : { status: 'failed', error: { code: 'InternalError', message: 'storage down' } as never }

    const report = settle(run, NOW)

    expect(report.watches.error).toBe('storage down')
    expect(report.skills.error).toBe('storage down')
    expect(report.settled_at).toBe(new Date(NOW).toISOString())
  })

  it('fires a due silence Watch once and counts a refused fire as a conflict', () => {
    const due = (id: string): unknown[] => [
      id,
      'quarterly check-in',
      {
        status: 'armed',
        watch_class: 'silence',
        due_at: '2026-09-01T00:00:00Z',
        condition: { after: '2026-09-01T00:00:00Z' },
      },
      1,
    ]
    const writes: KipOperation[] = []
    const run = (operation: KipOperation): KipResult => {
      if (operation.command.startsWith('UPDATE ?c')) return OK
      if (operation.command.includes('type: "Watch"')) return rows([due('C-7'), due('C-8')])
      if (operation.command.includes('type: "Skill"')) return rows([])
      writes.push(operation)
      return operation.parameters?.watch === 'C-8'
        ? { status: 'failed', error: { code: 'PreconditionFailed', message: 'moved' } as never }
        : OK
    }

    const report = settle(run, NOW)

    expect(report.watches).toEqual({ fired: 1, conflicted: 1 })
    // Idempotent under a concurrent evaluator: the key names the deadline, so
    // two sweeps that saw the same passed date resolve to one `watch_fire`.
    expect(writes[0]?.parameters?.fire_key).toBe(
      'watch_fire:C-7:silence:2026-09-01T00:00:00Z',
    )
    // A structured condition is carried as JSON, never flattened to the empty
    // string — "this Watch declares no condition" is the one thing it never is.
    expect(writes[0]?.command).toContain('status: "fired"')
  })
})
