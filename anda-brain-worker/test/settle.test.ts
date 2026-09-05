/**
 * The deterministic settlement, driven through its own port.
 *
 * `settle` takes a `RunKip` rather than reaching for a Durable Object, so the
 * verdict rule can be exercised across every arm without building five graphs
 * to produce five outcome streams. The HTTP suite still covers one path against
 * a real nexus; what is here is the rule table, which is the half that has to
 * agree with `anda_brain/src/settlement/skill.rs` decision for decision.
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
  /** Change Envelopes, as `CHANGES AFTER SEQ` answers them. */
  changes?: unknown[]
  /** The Propositions of the one slot a structured Watch names. */
  slot?: string[]
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
    if (command.startsWith('CHANGES AFTER SEQ')) return rows(graph.changes ?? [])
    if (command.includes('?p (:subject')) return rows(graph.slot ?? [])
    if (command.includes('"superseded"')) return rows([])
    if (command.startsWith('LIST DEPENDENTS')) return rows([])
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
    // Prose conditions, already past the guard: the Brain has consumed the
    // stream through the head at which each deadline was first seen passed.
    const due = (id: string): unknown[] => [
      id,
      'quarterly check-in',
      {
        status: 'armed',
        watch_class: 'silence',
        due_at: '2026-09-01T00:00:00Z',
        condition: { after: '2026-09-01T00:00:00Z' },
        due_seen_seq: 35,
      },
      1,
      30,
    ]
    const writes: KipOperation[] = []
    const run = (operation: KipOperation): KipResult => {
      if (operation.command.startsWith('UPDATE ?c')) return OK
      if (operation.command.includes('type: "Watch"')) return rows([due('C-7'), due('C-8')])
      if (operation.command.includes('type: "Skill"')) return rows([])
      if (operation.command.includes('"superseded"')) return rows([])
      writes.push(operation)
      return operation.parameters?.watch === 'C-8'
        ? { status: 'failed', error: { code: 'PreconditionFailed', message: 'moved' } as never }
        : OK
    }

    const report = settle(run, NOW, { headSeq: 40, consumedSeq: 39 })

    expect(report.watches).toEqual({ fired: 1, conflicted: 1, disarmed: 0, deferred: 0 })
    // Idempotent under a concurrent evaluator: the key names the deadline, so
    // two sweeps that saw the same passed date resolve to one `watch_fire`.
    expect(writes[0]?.parameters?.fire_key).toBe(
      'watch_fire:C-7:silence:2026-09-01T00:00:00Z',
    )
    // A structured-looking condition with a member this runtime cannot read
    // (`after`) is the Brain's, not the runtime's: it is carried as JSON and
    // held to the prose guard, never evaluated as a filter.
    expect(writes[0]?.command).toContain('status: "fired"')
    expect(writes[0]?.parameters?.evaluated_seq).toBe(39)
  })

  it('holds a prose silence Watch until the Brain has consumed its deadline', () => {
    // §5.11: the clock alone proves nothing. A matching change committed
    // before the deadline may still be waiting for the model whose job it is
    // to read a condition written in prose.
    const prose = (dueSeen?: number): unknown[] => [
      'C-7',
      'invoice acknowledgement',
      {
        status: 'armed',
        watch_class: 'silence',
        due_at: '2026-09-01T00:00:00Z',
        condition: 'no acknowledgement from billing',
        ...(dueSeen === undefined ? {} : { due_seen_seq: dueSeen }),
      },
      1,
      30,
    ]
    const sweep = (watch: unknown[], position: { headSeq?: number; consumedSeq?: number }) => {
      const { run, writes } = fakeRun({ watches: [watch] })
      const report = settle(run, NOW, position)
      return { report: report.watches, writes }
    }

    // First sight of the passed deadline: the head is recorded, nothing fires.
    let sweep1 = sweep(prose(), { headSeq: 40 })
    expect(sweep1.report).toEqual({ fired: 0, conflicted: 0, disarmed: 0, deferred: 1 })
    expect(sweep1.writes).toHaveLength(1)
    expect(sweep1.writes[0]?.command).toContain('due_seen_seq: :seq')
    expect(sweep1.writes[0]?.parameters?.seq).toBe(40)

    // The Brain has read the stream only up to before that head: still held,
    // and nothing is written twice.
    sweep1 = sweep(prose(40), { headSeq: 41, consumedSeq: 39 })
    expect(sweep1.report).toEqual({ fired: 0, conflicted: 0, disarmed: 0, deferred: 1 })
    expect(sweep1.writes).toHaveLength(0)

    // Consumed through it: silence is a fact, and the Watch fires.
    sweep1 = sweep(prose(40), { headSeq: 42, consumedSeq: 40 })
    expect(sweep1.report).toEqual({ fired: 1, conflicted: 0, disarmed: 0, deferred: 0 })
    expect(sweep1.writes[0]?.parameters?.fire_key).toBe(
      'watch_fire:C-7:silence:2026-09-01T00:00:00Z',
    )

    // With no head to record against, the Watch is held without a write.
    sweep1 = sweep(prose(), {})
    expect(sweep1.report).toEqual({ fired: 0, conflicted: 0, disarmed: 0, deferred: 1 })
    expect(sweep1.writes).toHaveLength(0)
  })

  /** A Watch scan row: `(id, name, attributes, version)`. */
  const watchRow = (id: string, attributes: Record<string, unknown>): unknown[] => [
    id,
    'the watch',
    { status: 'armed', ...attributes },
    3,
  ]

  /** The stream entry that armed a Watch. */
  const armed = (id: string): Record<string, unknown> => ({ op: 'create', kind: 'concept', id })

  /** One Change Envelope carrying one entry. */
  const envelope = (seq: number, entry: Record<string, unknown>): unknown => ({
    space_seq: seq,
    tx_id: `tx-${seq}`,
    changes: [entry],
  })

  it('fires a structured delta Watch on the change it watches, and only that', () => {
    const { run, writes } = fakeRun({
      watches: [watchRow('W-1', { watch_class: 'delta', condition: { element: 'C-42' } })],
      changes: [
        // Before the Watch was armed: not its business.
        envelope(9, { op: 'update', kind: 'concept', id: 'C-42' }),
        envelope(10, armed('W-1')),
        envelope(11, { op: 'update', kind: 'concept', id: 'C-7' }),
        envelope(12, { op: 'update', kind: 'concept', id: 'C-42', touched: ['attributes.name'] }),
      ],
    })

    const report = settle(run, NOW, { headSeq: 12 })

    expect(report.watches).toEqual({ fired: 1, conflicted: 0, disarmed: 0, deferred: 0 })
    expect(writes).toHaveLength(1)
    expect(writes[0]?.command).toContain('matched_seq: :matched_seq')
    expect(writes[0]?.parameters).toMatchObject({
      watch: 'W-1',
      version: 3,
      matched_seq: 12,
      fire_key: 'watch_fire:W-1:delta:12',
    })
  })

  it('stands a structured silence Watch down on a match, and fires it on none', () => {
    const slotWatch = watchRow(
      'W-2',
      {
        watch_class: 'silence',
        due_at: '2026-09-01T00:00:00Z',
        condition: { slot: { subject: 'C-1', predicate: 'replied_about' } },
      },
    )
    // The awaited reply arrived: the Watch stands down without firing.
    let sweep = fakeRun({
      watches: [slotWatch],
      slot: ['P-11'],
      changes: [
        envelope(11, { op: 'create', kind: 'assertion', id: 'A-3', refs: { proposition: 'P-11' } }),
      ],
    })
    let report = settle(sweep.run, NOW, { headSeq: 12 })
    expect(report.watches).toEqual({ fired: 0, conflicted: 0, disarmed: 1, deferred: 0 })
    expect(sweep.writes).toHaveLength(1)
    expect(sweep.writes[0]?.command).toContain('status: "disarmed"')
    expect(sweep.writes[0]?.parameters?.matched_seq).toBe(11)

    // Nothing matched through the head: silence, concluded over a consumed
    // stream, fires in the same sweep.
    sweep = fakeRun({ watches: [slotWatch], slot: ['P-11'], changes: [] })
    report = settle(sweep.run, NOW, { headSeq: 12 })
    expect(report.watches).toEqual({ fired: 1, conflicted: 0, disarmed: 0, deferred: 0 })
    expect(sweep.writes).toHaveLength(1)
    expect(sweep.writes[0]?.command).toContain('evaluated_seq: :evaluated_seq')
    expect(sweep.writes[0]?.parameters?.evaluated_seq).toBe(12)
  })

  it('records where a structured Watch read to when nothing matched', () => {
    const { run, writes } = fakeRun({
      watches: [watchRow('W-3', { watch_class: 'delta', condition: { type: 'Commitment' } })],
      changes: [
        envelope(11, {
          op: 'create',
          kind: 'concept',
          id: 'C-9',
          schema_ref: 'kip://profiles/cognitive-memory@2.0.0/Person',
        }),
      ],
    })

    const report = settle(run, NOW, { headSeq: 11 })

    expect(report.watches).toEqual({ fired: 0, conflicted: 0, disarmed: 0, deferred: 0 })
    expect(writes).toHaveLength(1)
    expect(writes[0]?.command).toContain('evaluated_seq: :seq')
    expect(writes[0]?.parameters?.seq).toBe(11)
  })

  it('walks the dependents of each newly superseded claim, and moves the cursor', () => {
    const writes: KipOperation[] = []
    const asked: string[] = []
    const run = (operation: KipOperation): KipResult => {
      asked.push(operation.command)
      if (operation.command.startsWith('UPDATE ?c')) return OK
      if (operation.command.includes('type: "Watch"')) return rows([])
      if (operation.command.includes('type: "Skill"')) return rows([])
      if (operation.command.includes('"superseded"')) {
        expect(operation.parameters?.after).toBe(7)
        return rows([['A-1', 9, { id: 'C-2' }, { id: 'P-5' }, [{ id: 'A-2' }]]])
      }
      if (operation.command.startsWith('LIST DEPENDENTS')) {
        expect(operation.parameters?.root).toBe('A-1')
        return rows([{ id: 'C-30', kind: 'concept', distance: 1, via: { activity: 'ACT-4' } }])
      }
      writes.push(operation)
      return OK
    }

    const report = settle(run, NOW, { correctionCursor: 7 })

    expect(report.corrections).toEqual({
      cursor: 9,
      revised_roots: [
        {
          assertion: 'A-1',
          space_seq: 9,
          actor: 'C-2',
          proposition: 'P-5',
          superseded_by: ['A-2'],
          dependents: [{ id: 'C-30', kind: 'concept', distance: 1, via: 'ACT-4' }],
          truncated: false,
        },
      ],
    })
    // Reachability is topology, not judgment: nothing was flagged stale.
    expect(writes).toHaveLength(0)
  })
})
