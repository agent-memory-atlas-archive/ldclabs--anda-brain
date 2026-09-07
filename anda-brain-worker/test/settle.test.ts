import type { KipOperation } from '../src/kip.js'
const OK: KipResult = {status:'succeeded'}
import { KipError, type KipResult } from '@ldclabs/kip-do'
import { describe, expect, it } from 'vitest'
import { settle } from '../src/settle.js'
const NOW = Date.parse('2026-09-03T00:00:00Z')
const rows = (result: unknown[]): KipResult => ({ status: 'succeeded', result: result as never })

describe('deterministic settlement', () => {
  it('leaves learning unsupported and never tallies self-reported success', () => {
    const commands: string[] = []
    const report = settle((op) => { commands.push(op.command); return rows([]) }, NOW)
    expect(report.skills.transitions).toBe(0)
    expect(report.skills.unsupported_reason).toContain('independent observers')
    expect(commands.every((command) => !command.includes('GradingState') && !command.includes('OutcomeRecord'))).toBe(true)
  })
  it('defers prose and mixed text selectors without manufacturing a watermark', () => {
    const report = settle((op) => op.command.includes('type: "Watch"') ? rows([
      ['C-1','wait',{condition:'no reply'},2,{arm_generation:1}],
      ['C-2','wait',{condition:{element:'C-1',text:'reply'}},3,{arm_generation:2}],
    ]) : rows([]), NOW, { advanceWatch: () => { throw new Error('must not evaluate text') } })
    expect(report.watches).toEqual({fired:0,disarmed:0,deferred:2,conflicted:0})
  })
  it('reports a failed decay without suppressing protected Watch advancement', () => {
    const report = settle((op) => {
      if (op.command.startsWith('UPDATE ?c')) return {status:'failed',error:new KipError('InternalError','scan budget exceeded').toJSON()}
      if (op.command.includes('type: "Watch"')) return rows([
        ['C-1','wait',{condition:{element:'C-2'}},7,{arm_generation:3}],
      ])
      return rows([])
    }, NOW, {advanceWatch: () => ({status:'fired'})})
    expect(report.decayed).toBe(0)
    expect(report.decay_error).toBe('scan budget exceeded')
    expect(report.watches.fired).toBe(1)
  })
  it('passes the real version and generation to the protected runtime', () => {
    const calls: unknown[] = []
    const report = settle((op) => op.command.includes('type: "Watch"') ? rows([
      ['C-1','wait',{condition:{element:'C-2'}},7,{arm_generation:3}],
    ]) : rows([]), NOW, { advanceWatch: (...args) => { calls.push(args); return {status:'fired'} } })
    expect(calls).toEqual([['C-1',7,3]])
    expect(report.watches.fired).toBe(1)
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
      incomplete: false,
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
