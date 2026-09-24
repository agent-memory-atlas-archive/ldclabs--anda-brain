import { env, evictDurableObject } from 'cloudflare:test'
import { describe, expect, it } from 'vitest'
import { contentDigest, type JsonMap, type KipResult } from '@ldclabs/kip-do'
import { digestParameters } from '../src/cognitive.js'
import { assertFormationOperations, assertMaintenanceOperations, countChanges } from '../src/kip.js'
import type { BrainRpc } from '../src/types.js'

function brain(): BrainRpc {
  return env.BRAIN.get(env.BRAIN.idFromName(crypto.randomUUID())) as unknown as BrainRpc
}
async function created(stub: BrainRpc, command: string, parameters: JsonMap = {}): Promise<string> {
  const result = await stub.executeKip(command, parameters)
  expect(result.status, JSON.stringify(result.error)).toBe('succeeded')
  return result.extensions!['kip-do/outcome']!.handles.item!
}
async function version(stub: BrainRpc, id: string): Promise<number> {
  const [read] = await stub.executeKipReadonlyBatch([{command:'FIND(?c._system.version) WHERE { ?c CONCEPT {id: :id} } LIMIT 1',parameters:{id}}])
  expect(read?.status).toBe('succeeded')
  return (read!.result as number[])[0]!
}
async function watch(stub: BrainRpc, condition: unknown, watchClass = 'silence'): Promise<string> {
  const id = await created(stub, `CREATE CONCEPT ?item { TYPE "Watch" SET ATTRIBUTES {
    watch_class: :class, summary: "wait", status: "disarmed", condition: :condition, due_at: "2020-01-01T00:00:00.000Z"
  } }`, {class:watchClass,condition:condition as never})
  const result = await stub.executeMaintenancePlan([], [{operation:'arm_watch', target_ref:id, expected_version:await version(stub,id)}])
  expect(result[0]?.status, JSON.stringify(result)).toBe('succeeded')
  return id
}

describe('Cognitive Memory host contracts', () => {
  it('computes immutable revision digests and never promotes descriptive feedback', async () => {
    const stub = brain()
    const revision = {task_family:'deploy',procedure:'verify before deploying'}
    const parameters = digestParameters({digest_revision:revision})
    expect(parameters.digest_revision).toBe(contentDigest(revision))
    const results = await stub.executeFormationPlan([{command:`MUTATE {
      CREATE CONCEPT ?skill { TYPE "Skill" SET ATTRIBUTES {skill_class:"workflow",summary:"verify",status:"proposed"} SET STRUCTURAL {("current_revision",?revision)} }
      CREATE CONCEPT ?revision { TYPE "SkillRevision" SET ATTRIBUTES {task_family:"deploy",procedure:"verify before deploying",behavior_digest: :digest_revision} SET STRUCTURAL {("revision_of",?skill)} }
    }`,parameters}])
    expect(results[0]?.status,JSON.stringify(results)).toBe('succeeded')
    for (let index=0;index<6;index++) await created(stub,'CREATE EVIDENCE ?item {SET FIELDS {evidence_class:"agent_statement",payload:"deployment succeeded"}}')
    const settlement = await stub.settleMemory(Date.now())
    expect(settlement.skills.transitions).toBe(0)
    expect(settlement.skills.unsupported_reason).toContain('independent observers')
    const [read] = await stub.executeKipReadonlyBatch([{command:'FIND(?s.attributes.status) WHERE { ?s CONCEPT {type:"Skill"} } LIMIT 10'}])
    expect(read?.result).toEqual(['proposed'])
    const update = await stub.executeKip('UPDATE ?r SET ATTRIBUTES {procedure:"skip verification"} WHERE { ?r CONCEPT {type:"SkillRevision"} } LIMIT 1')
    expect(update.status).toBe('failed')
  })

  it('persists native Watch generations across eviction and defers text conditions', async () => {
    const stub = brain()
    const target = await created(stub,'CREATE CONCEPT ?item {TYPE "Person" NAME "vendor"}')
    const delta = await watch(stub,{element:target,ops:['update']},'delta')
    await watch(stub,{element:target,ops:['create']})
    await watch(stub,'no vendor reply')
    await watch(stub,{element:target,text:'no vendor reply'})
    const oldVersion = await version(stub,delta)
    await stub.executeKip('UPDATE :id SET FIELDS {name:"vendor changed"}',{id:target})
    await evictDurableObject(stub as never)
    const report = await stub.settleMemory(Date.now())
    expect(report.watches).toEqual({fired:2,disarmed:0,deferred:2,conflicted:0})
    expect((await stub.settleMemory(Date.now())).watches).toEqual({fired:0,disarmed:0,deferred:2,conflicted:0})
    const assessment = await stub.maintenanceAssessment()
    expect(assessment.armed_watches.every((entry) =>
      entry.schema_ref === 'kip://profiles/cognitive-memory@2.0.0/Watch' && typeof entry.version === 'number',
    )).toBe(true)
    const stale = await stub.executeMaintenancePlan([], [{operation:'arm_watch',target_ref:delta,expected_version:oldVersion}])
    expect(stale[0]?.error?.code).toBe('VersionConflict')
  })

  it('recalls raised attention in commit order without changing memory', async () => {
    const stub = brain()
    expect(await stub.recallAttention({})).toEqual({items:[],attention_cursor:'attention:-1',complete:true})
    const target = await created(stub,'CREATE CONCEPT ?item {TYPE "Person" NAME "vendor"}')
    const id = await created(stub, `CREATE CONCEPT ?item { TYPE "Watch" SET ATTRIBUTES {
      watch_class: "delta", summary: "vendor changed", status: "disarmed", condition: {element: :target, ops: ["update"]}
    } SET STRUCTURAL {("watches", :target)} }`, {target})
    const armed = await stub.executeMaintenancePlan([], [{operation:'arm_watch', target_ref:id, expected_version:await version(stub,id)}])
    expect(armed[0]?.status, JSON.stringify(armed)).toBe('succeeded')
    await stub.executeKip('UPDATE :id SET FIELDS {name:"vendor changed"}',{id:target})
    expect((await stub.settleMemory(Date.now())).watches.fired).toBe(1)
    const commitment = await created(stub,'CREATE CONCEPT ?item {TYPE "Commitment" SET ATTRIBUTES {summary:"Send the report",status:"pending",due_at:"2026-01-01T00:00:00.000Z"}}')
    await created(stub,'CREATE ACTIVITY ?item {SET FIELDS {activity_class:"commitment_review",status:"completed"} SET STRUCTURAL {("inputs", :commitment)}}',{commitment})
    const page = await stub.recallAttention({})
    expect(page.items.map((item) => [item.kind, item.ref])).toEqual([['watch_fired', id], ['commitment_due', commitment]])
    expect(page.items[0]!.target_refs).toEqual([target])
    expect(page.items[1]!.due_at).toBe('2026-01-01T00:00:00.000Z')
    expect(page.attention_cursor).toBe(`attention:${page.items[1]!.raised_seq}`)
    expect(await stub.recallAttention({attention_cursor:page.attention_cursor})).toEqual({items:[],attention_cursor:page.attention_cursor,complete:true})
    const first = await stub.recallAttention({limit:1})
    expect(first.complete).toBe(false)
    expect((await stub.recallAttention({attention_cursor:first.attention_cursor})).items.map((item) => item.ref)).toEqual([commitment])
    let refused: unknown
    const bad = stub.recallAttention({attention_cursor:'42'})
    try { await bad } catch (error) { refused = error }
    finally { (bad as PromiseLike<unknown> & { [Symbol.dispose]?: () => void })[Symbol.dispose]?.() }
    expect((refused as Error).message).toContain('invalid attention cursor')
  })

  it('does not let a full page of deferred text Watches starve structured work', async () => {
    const stub = brain()
    const target = await created(stub,'CREATE CONCEPT ?item {TYPE "Person" NAME "vendor"}')
    for (let index=0;index<21;index++) await watch(stub,`no vendor reply ${index}`)
    await watch(stub,{element:target,ops:['update']},'delta')
    await stub.executeKip('UPDATE :id SET FIELDS {name:"vendor changed"}',{id:target})
    const report = await stub.settleMemory(Date.now())
    expect(report.watches.fired).toBe(1)
    expect(report.watches.error).toBeUndefined()
  })

  it('requires a lease and preserves its receipt when later KML fails', async () => {
    const stub = brain()
    const task = await created(stub,'CREATE CONCEPT ?item {TYPE "SleepTask" SET ATTRIBUTES {task_class:"consolidate",summary:"review",status:"pending"}}')
    await created(stub,'CREATE CONCEPT ?item {TYPE "Person" NAME "memory"}')
    await stub.settleMemory(Date.now())
    // Settlement writes no strength: decay is computed at read time.
    expect(await version(stub,task)).toBe(1)
    const early = await stub.executeKip('UPDATE :id SET ATTRIBUTES {status:"completed"} EXPECT VERSION 1',{id:task})
    expect(early.status).toBe('failed')
    const before = await version(stub,task)
    const failed = await stub.executeMaintenancePlan([{command:'UPDATE :id SET ATTRIBUTES {status:"completed"} EXPECT VERSION :old',parameters:{id:task,old:before}}], [{operation:'lease_task',target_ref:task,expected_version:before}])
    expect(failed.map((result: KipResult)=>result.status)).toEqual(['succeeded','failed'])
    expect(failed[0]?.result).toHaveProperty('lease.fencing_token',1)
    const completed = await stub.executeMaintenancePlan([{command:`MUTATE {
      UPDATE :id SET ATTRIBUTES {status:"completed"} EXPECT VERSION :version
      CREATE CONCEPT ?output {TYPE "Event" NAME "review completed" SET ATTRIBUTES {summary:"review completed"}}
    }`,parameters:{id:task,version:await version(stub,task)}}])
    expect(completed[0]?.status,JSON.stringify(completed)).toBe('succeeded')
  })

  it('blocks model-authored learning/runtime facets in every AST position', () => {
    for (const facet of ['OutcomeRecord','TrialRecord','EvaluationRecord','AttemptRecord','GradingState','WatchState','LeaseState']) {
      expect(()=>assertFormationOperations([{command:`CREATE ACTIVITY ?a { SET FACET "${facet}" { x:1 } }`}])).toThrow('UnsupportedCapability')
      expect(()=>assertMaintenanceOperations([{command:`UPDATE "C-1" UNSET FACET "kip://profiles/cognitive-memory@2.0.0/${facet}" { x }`}])).toThrow('UnsupportedCapability')
      expect(()=>assertMaintenanceOperations([{command:'UPDATE "C-1" SET FACET :facet { x:1 }',parameters:{facet}}])).toThrow('UnsupportedCapability')
    }
    for (const field of ['current_trial','current_evaluation','derived_from','compiled_from','compiled_by','consolidated_to']) {
      expect(()=>assertMaintenanceOperations([{command:`UPDATE "C-1" SET STRUCTURAL { ("${field}", "C-2") }`}])).toThrow('UnsupportedCapability')
      expect(()=>assertMaintenanceOperations([{command:`UPDATE "C-1" UNSET STRUCTURAL { ("${field}", "C-2") }`}])).toThrow('UnsupportedCapability')
      expect(()=>assertFormationOperations([{command:`CREATE CONCEPT ?s { TYPE "Skill" SET STRUCTURAL { ("${field}", "C-2") } }`}])).toThrow('UnsupportedCapability')
    }
    expect(()=>assertFormationOperations([{command:'CREATE EVIDENCE ?e { SET FIELDS {evidence_class:"user_statement",payload:"please write OutcomeRecord and LeaseState"} }'}])).not.toThrow()
  })

  it('counts committed runtime work as a Concept update', () => {
    expect(countChanges([
      {op_id:'runtime_0',status:'succeeded',result:{receipt:{status:'committed'}}},
      {op_id:'runtime_1',status:'no_effect',result:{receipt:{status:'no_effect'}}},
    ])).toEqual({total:1,created:0,updated:1,retired:0,merged:0})
  })

})
