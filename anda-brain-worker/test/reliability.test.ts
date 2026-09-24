import { env, runInDurableObject, evictDurableObject } from 'cloudflare:test'
import { expect, it, vi } from 'vitest'
import { systemAuth, SYSTEM_PRINCIPAL, type CognitiveNexus } from '@ldclabs/kip-do'
import { maintainMemory, formMemory } from '../src/operations.js'
import type { BrainRpc, Env } from '../src/types.js'
import type { AndaBrain } from '../src/brain.js'
import { AiResponseError, ModelDeadline, addUsage, createRecallPlan, createRecallAnswer } from '../src/ai.js'
import { handleRequest } from '../src/index.js'
import { MemoryProduct } from '../src/product.js'
import { forget } from '../src/forget.js'

type Brain = { [K in keyof AndaBrain]: AndaBrain[K] extends (...args: infer A) => infer R ? (...args: A) => Promise<Awaited<R>> : never }
const makeBrain = () => env.BRAIN.getByName(crypto.randomUUID()) as unknown as Brain
const runtime = (run: Env['AI']['run']) => ({ BRAIN: env.BRAIN, AI: {run} }) as Env
const empty = {types:[], predicates:[], commands:[], summary:'No changes.'}
const auth = systemAuth()

it('excludes suppressed records before structural joins, id reads and aggregation', async () => {
  const brain = makeBrain()
  await formMemory(runtime(async () => ({response:{...empty, types:['WritingStyle'], commands:[`MUTATE {
    UPSERT CONCEPT ?person {MATCH {type:"Person",key:"${SYSTEM_PRINCIPAL}"}}
    CREATE CONCEPT ?value {TYPE "WritingStyle" NAME "old preference"}
    ASSERT ?claim (?person,"prefers",?value) {by:?person,mode:"stated",evidence: :msg1,at:"2026-09-22T00:00:00.000Z"}
  }`]}})), brain as unknown as BrainRpc, {messages:[{role:'user',content:'private source text'}]})
  const record = (await brain.productRecords(auth)).records[0]!
  const preview = await brain.productPrepare(auth,{operation_id:'hide',record_id:record.id,expected_revision:record.revision,kind:'suppress'})
  expect((await brain.productCommit(auth,'hide',preview.preview_digest)).state).toBe('confirmed')
  const epoch = await brain.beginProcessing()
  const results = await brain.executeAgentRead([{command:'FIND(?e.payload) WHERE { STRUCTURAL (?a,"evidence",?e) } LIMIT 20'}],epoch)
  expect(results[0]!.status).toBe('succeeded')
  expect(results[0]!.result).toEqual([])
  const counts = await brain.executeAgentRead([{command:'FIND(COUNT(?e)) WHERE { STRUCTURAL (?a,"evidence",?e) } LIMIT 1'}],epoch)
  expect(counts[0]!.result).toEqual([0])
  const audit = await brain.executeKipReadonlyBatch([{command:'FIND(?e.payload) WHERE { STRUCTURAL (?a,"evidence",?e) } LIMIT 20'}])
  expect(JSON.stringify(audit)).toContain('private source text')
  const proposition = await brain.executeAgentRead([{command:`FIND(?p) WHERE {?p (id:"${record.proposition_id}")} LIMIT 1`}],epoch)
  expect(proposition[0]!.result).toEqual([])
  const hiddenWrite = await brain.executeMaintenancePlan([{
    command:'TRANSITION ?a TO "retracted" WHERE { STRUCTURAL (?a,"evidence",?e) } LIMIT 20',
  }],[],epoch)
  expect(hiddenWrite[0]!.status,JSON.stringify(hiddenWrite)).toBe('no_effect')
  const fresh = await brain.executeKip('CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:"current source"}}')
  expect(fresh.status).toBe('succeeded')
  expect(JSON.stringify(await brain.executeAgentRead([{command:'FIND(?e.payload) WHERE {?e EVIDENCE {}} LIMIT 20'}],epoch))).toContain('current source')
  const formed = await formMemory(runtime(async () => ({response:{...empty,commands:[`MUTATE {
    CREATE CONCEPT ?value {TYPE "WritingStyle" NAME "fresh preference"}
    UPSERT CONCEPT ?person {MATCH {type:"Person",key:"${SYSTEM_PRINCIPAL}"}}
    ASSERT ?claim (?person,"prefers",?value) {by:?person,mode:"stated",evidence: :msg1,at:"2026-09-22T00:00:00.000Z"}
  }`]}})),brain as unknown as BrainRpc,{messages:[{role:'user',content:'fresh independent observation'}]}) as any
  expect(formed.stored.assertions).toBe(1)
  const search = await brain.executeAgentRead([{command:'SEARCH CONCEPT "fresh preference" LIMIT 5'}],epoch)
  expect(search[0]!.status,JSON.stringify(search)).toBe('succeeded')
  expect(JSON.stringify(search)).toContain('fresh preference')
  await expect((async()=>await brain.executeAgentRead([{command:'CREATE CONCEPT ?x {TYPE "Person" NAME "forbidden write"}'}],epoch))()).rejects.toThrow('read-only')
  const nested = await brain.executeAgentRead([{command:`FIND(?p) WHERE {
    OPTIONAL {?p (id:"${record.proposition_id}")}
  } LIMIT 1`}],epoch)
  expect(JSON.stringify(nested)).not.toContain('archived')
})

it('retains correction pages through model failure and eviction until explicitly acknowledged', async () => {
  const brain = makeBrain()
  expect((await brain.settleMemory(Date.now())).corrections.revised_roots).toEqual([])
  await brain.declareSymbols(['WritingStyle'],[])
  const seed = await brain.executeKip(`MUTATE {
    CREATE CONCEPT ?person {TYPE "Person" NAME "Actor"}
    CREATE CONCEPT ?value {TYPE "WritingStyle" NAME "Plain"}
    ASSERT ?old (?person,"prefers",?value) {by:?person,mode:"stated"}
    ASSERT ?replacement (?person,"prefers",?value) {by:?person,mode:"stated"}
  }`)
  expect(seed.status,JSON.stringify(seed)).toBe('succeeded')
  const handles = seed.extensions!['kip-do/outcome']!.handles
  const superseded = await brain.executeKip('TRANSITION :old TO "superseded" BY :replacement', {old:handles.old!,replacement:handles.replacement!})
  expect(superseded.status,JSON.stringify(superseded)).toBe('succeeded')
  const snapshots: any[] = []
  const ai = runtime(async (_model,input) => {
    snapshots.push(JSON.parse((input.messages as any[])[1].content).snapshot)
    if (snapshots.length === 1) throw new Error('temporary provider failure')
    if (snapshots.length === 2) return {response:empty}
    return {response:{...empty,reviewed_corrections:snapshots.at(-1).assessment.revised_roots.map((root:any)=>root.assertion)}}
  })
  await expect(maintainMemory(ai,brain as unknown as BrainRpc,{})).rejects.toThrow('temporary provider failure')
  await evictDurableObject(brain as never)
  await maintainMemory(ai,brain as unknown as BrainRpc,{})
  await maintainMemory(ai,brain as unknown as BrainRpc,{})
  await maintainMemory(ai,brain as unknown as BrainRpc,{})
  expect(snapshots[0].assessment.revised_roots).toHaveLength(1)
  expect(snapshots[1].assessment.revised_roots).toHaveLength(1)
  expect(snapshots[2].assessment.revised_roots).toHaveLength(1)
  expect(snapshots[3].assessment.revised_roots).toHaveLength(0)
})

it('excludes completed SleepTasks from maintenance work', async () => {
  const brain = makeBrain()
  for (let i=0;i<20;i++) {
    const created = await brain.executeKip('CREATE CONCEPT ?task {TYPE "SleepTask" SET ATTRIBUTES {task_class:"consolidate",summary:"done",status:"pending"}}')
    expect(created.status).toBe('succeeded')
    const task = created.extensions!['kip-do/outcome']!.handles.task!
    expect((await brain.executeMaintenancePlan([],[{operation:'lease_task',target_ref:task,expected_version:1}]))[0]!.status).toBe('succeeded')
    const complete = await brain.executeKip('UPDATE :task SET ATTRIBUTES {status:"completed"} EXPECT VERSION 2',{task})
    expect(complete.status,JSON.stringify(complete)).toBe('succeeded')
  }
  const added = await brain.executeKip('CREATE CONCEPT ?pending {TYPE "SleepTask" SET ATTRIBUTES {task_class:"consolidate",summary:"not done",status:"pending"}}')
  expect(added.status).toBe('succeeded')
  let snapshot: any
  await maintainMemory(runtime(async (_model,input) => {
    snapshot = JSON.parse((input.messages as any[])[1].content).snapshot
    return {response:empty}
  }),brain as unknown as BrainRpc,{})
  const rows = snapshot.snapshot[1].result
  expect(rows).toHaveLength(1)
  expect(rows[0].attributes.status).toBe('pending')
})

it('reports no_effect rather than a model-authored success sentence', async () => {
  const brain = makeBrain()
  const result = await maintainMemory(runtime(async () => ({response:{...empty,
    summary:'Archived the obsolete event.',
    commands:['TRANSITION ?e TO "archived" WHERE {?e CONCEPT {type:"Event",name:"missing"}} LIMIT 1'],
  }})),brain as unknown as BrainRpc,{}) as any
  expect(result.changed.total).toBe(0)
  expect(result.content).toBe('No maintenance-plan changes were committed.')
  expect(result.operation_results).toMatchObject([{status:'no_effect'}])
})

it('provides Event content, versions and the live primer to maintenance',async()=>{
  const brain=makeBrain()
  expect((await brain.executeKip('CREATE CONCEPT ?event {TYPE "Event" NAME "Meeting" SET ATTRIBUTES {summary:"unique-critical-decision"}}')).status).toBe('succeeded')
  let payload:any
  await maintainMemory(runtime(async (_model,input)=>{
    payload=JSON.parse((input.messages as any[])[1].content)
    return {response:empty}
  }),brain as unknown as BrainRpc,{})
  expect(JSON.stringify(payload)).toContain('Meeting')
  expect(JSON.stringify(payload)).toContain('unique-critical-decision')
  expect(payload.primer).toHaveProperty('cognitive_identity')
  expect(payload.snapshot.snapshot[0].result[0]._system.version).toBeGreaterThan(0)
})

it('groups predicate counts in one query and bounds snapshot candidates before KQL', async () => {
  const brain = makeBrain()
  await brain.declareSymbols(['WritingStyle'],Array.from({length:16},(_,i)=>`relation_${i}`))
  await runInDurableObject(brain as never,(instance)=>{
    const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
    const original = nexus.execute.bind(nexus)
    for(let i=0;i<100;i++) original(`CREATE CONCEPT ?c {TYPE "WritingStyle" NAME "memory ${i}"}`)
    const all = nexus.store.all.bind(nexus.store)
    const calls: {table:string;count:number}[] = []
    nexus.store.all = ((...args: Parameters<typeof all>) => {
      const result = all(...args)
      calls.push({table:args[0],count:result.length})
      return result
    }) as typeof nexus.store.all
    try {
      const assessment = (instance as unknown as AndaBrain).maintenanceAssessment()
      const predicateReads = calls.filter(c=>c.table==='propositions').length
      expect(predicateReads).toBe(1)
      expect(Object.keys(assessment.predicates)).toHaveLength(19)
      calls.length=0
      const host = instance as unknown as AndaBrain
      const run = host.beginMaintenance(0, Date.now()+60_000)
      const snapshot = host.maintenanceSnapshot(0,run)
      host.endMaintenance(run)
      expect(snapshot[2]!.result).toHaveLength(20)
      expect(calls.filter(c=>c.table==='concepts').every(c=>c.count<=20)).toBe(true)

    } finally { nexus.store.all=all }
  })
})

it('loads only the active schema packages when inspecting vocabulary', async () => {
  const brain=makeBrain()
  for(let i=0;i<8;i++) await brain.declareSymbols([`Project${i}`],[])
  await runInDurableObject(brain as never,(instance)=>{
    const nexus=(instance as unknown as {nexus:CognitiveNexus}).nexus
    const allRows=nexus.store.all.bind(nexus.store)
    let loaded=0, bytes=0
    nexus.store.all=((...args: Parameters<typeof allRows>)=>{
      const all=allRows(...args)
      if(args[0]==='schema_packages') {
        loaded+=all.length
        bytes+=new TextEncoder().encode(JSON.stringify(all)).length
      }
      return all
    }) as typeof nexus.store.all
    try {
      const result=(instance as unknown as AndaBrain).vocabulary()
      expect(result.draft_types).toHaveLength(8)
      expect(loaded).toBe(0)
      expect(bytes).toBe(0)
    } finally {nexus.store.all=allRows}
  })
})

it('keeps drafted vocabulary and one review per symbol across an eviction', async () => {
  const brain = makeBrain()
  expect(await brain.declareSymbols(['Project'], ['works_on', 'bad name'])).toMatchObject({
    package_ref: null, draft_types: ['Project'], draft_predicates: ['works_on'],
    defined: ['kip://local/draft@0.0.0/Project', 'kip://local/draft@0.0.0/works_on'], rejected: ['bad name'],
  })
  await evictDurableObject(brain as never)
  // Already drafted: nothing new, and the review is not queued twice.
  expect(await brain.declareSymbols(['Project', 'Milestone'], [])).toMatchObject({
    draft_types: ['Milestone', 'Project'], defined: ['kip://local/draft@0.0.0/Milestone'], rejected: [],
  })
  const [reviews] = await brain.executeKipReadonlyBatch([{command:`FIND(?t.attributes.symbol_kind, ?t.attributes.symbol_ref) WHERE {
    ?t CONCEPT {type: "SleepTask"}
    FILTER(?t.attributes.task_class == "review_schema")
  } ORDER BY ?t.attributes.symbol_ref ASC LIMIT 10`}])
  expect(reviews!.result).toEqual([
    ['ConceptType', 'kip://local/draft@0.0.0/Milestone'],
    ['ConceptType', 'kip://local/draft@0.0.0/Project'],
    ['PredicateType', 'kip://local/draft@0.0.0/works_on'],
  ])
})

it('persists maintenance admission and fences an expired owner after takeover', async () => {
  const brain = makeBrain()
  const first = await brain.beginMaintenance(0, Date.now()+60_000)
  await evictDurableObject(brain as never)
  await expect((async()=>await brain.beginMaintenance(0,Date.now()+60_000))()).rejects.toThrow('maintenance_busy')
  await runInDurableObject(brain as never, (_instance, state) => {
    const key = 'anda-brain:maintenance_run'
    state.storage.kv.put(key, {...state.storage.kv.get<any>(key),expires_at:0})
  })
  const second = await brain.beginMaintenance(0, Date.now()+60_000)
  await expect((async()=>await brain.executeMaintenancePlan([],[],0,first))()).rejects.toThrow('maintenance_run_expired')
  await brain.endMaintenance(first)
  await expect((async()=>await brain.beginMaintenance(0,Date.now()+60_000))()).rejects.toThrow('maintenance_busy')
  expect(await brain.executeMaintenancePlan([],[],0,second)).toEqual([])
  await brain.endMaintenance(second)
})

it('rotates pending tasks beyond the first page', async () => {
  const brain = makeBrain()
  for (let i=0;i<21;i++) {
    expect((await brain.executeKip('CREATE CONCEPT ?task {TYPE "SleepTask" SET ATTRIBUTES {task_class:"consolidate",summary:"pending",status:"pending"}}')).status).toBe('succeeded')
  }
  const run = await brain.beginMaintenance(0, Date.now()+60_000)
  const first = await brain.maintenanceSnapshot(0,run)
  const second = await brain.maintenanceSnapshot(0,run)
  expect(first[1]!.result).toHaveLength(20)
  expect(second[1]!.result).toHaveLength(1)
  const ids = [...first[1]!.result as any[], ...second[1]!.result as any[]].map(row=>row.id)
  expect(new Set(ids).size).toBe(21)
  await brain.endMaintenance(run)
})

it('preserves unknown usage and the known subtotal across reference calls', async () => {
  let calls = 0
  const result = await createRecallPlan({async run() {
    return ++calls === 1 ? {response:{commands:[],references:[{document:'syntax',section:'kql',offset:0}]},usage:{input_tokens:10,output_tokens:5}}
      : {response:{commands:[]}}
  }},'fixture',[])
  expect(result.usage).toEqual({input_tokens:null,output_tokens:null,known:{input_tokens:10,output_tokens:5}})
  const zero = await createRecallPlan({async run(){return {response:{commands:[]},usage:{input_tokens:0,output_tokens:0}}}},'fixture',[])
  expect(zero.usage).toEqual({input_tokens:0,output_tokens:0})
})

it('shares a deadline across stages and cancels a provider that ignores the signal', async () => {
  vi.useFakeTimers()
  const deadline = new ModelDeadline('100')
  try {
    const plan = await createRecallPlan({async run(){return {response:{commands:[]},usage:{input_tokens:10,output_tokens:5}}}},'fixture',[],deadline)
    await vi.advanceTimersByTimeAsync(60)
    let signal: AbortSignal | undefined
    const pending = createRecallAnswer({run(_model,_input,options){signal=options?.signal;return new Promise(()=>{})}},'fixture',[],deadline).catch(error=>error)
    await vi.advanceTimersByTimeAsync(40)
    const error = await pending as AiResponseError
    expect(error.code).toBe('model_timeout')
    expect(signal?.aborted).toBe(true)
    expect(addUsage(plan.usage,error.usage)).toMatchObject({input_tokens:null,known:{input_tokens:10,output_tokens:5}})
  } finally { deadline.close(); vi.useRealTimers() }
})

it('never executes a late Formation response after its deadline', async () => {
  const brain = makeBrain()
  await brain.stats()
  let release!: (response: unknown)=>void
  const ai = runtime(() => new Promise(resolve=>{release=resolve}))
  ai.AI_TIMEOUT_MS='100'
  await expect(formMemory(ai,brain as unknown as BrainRpc,{messages:[{role:'user',content:'late input'}]})).rejects.toMatchObject({code:'model_timeout'})
  release?.({response:{...empty,commands:['CREATE CONCEPT ?late {TYPE "Person" NAME "late write"}']}})
  await Promise.resolve()
  const result = await brain.executeKip('FIND(?p) WHERE {?p CONCEPT {type:"Person",name:"late write"}} LIMIT 20')
  expect(result.result).toEqual([])
})

it('does not classify a provider message containing a processing code as a conflict', async () => {
  const response = await handleRequest(new Request(`https://test/v1/${crypto.randomUUID()}/formation`,{
    method:'POST',body:JSON.stringify({messages:[{role:'user',content:'input'}]}),
  }),runtime(async()=>{throw new Error('source_suppressed')}))
  expect(response.status).toBe(502)
  expect(await response.json()).toMatchObject({error:{data:{code:'model_call_failed'}}})
})

it('aggregates erasure cleanup and scrubs every preview page, including expired drafts', async () => {
  const brain = makeBrain()
  await formMemory(runtime(async()=>({response:{...empty,types:['WritingStyle'],commands:[`MUTATE {
    UPSERT CONCEPT ?person {MATCH {type:"Person",key:"${SYSTEM_PRINCIPAL}"}}
    CREATE CONCEPT ?value {TYPE "WritingStyle" NAME "old"}
    ASSERT ?claim (?person,"prefers",?value) {by:?person,mode:"stated",evidence: :msg1,at:"2026-09-22T00:00:00.000Z"}
  }`]}})),brain as unknown as BrainRpc,{messages:[{role:'user',content:'source'}]})
  const record = (await brain.productRecords(auth)).records[0]!
  const preview = await brain.productPrepare(auth,{operation_id:'draft',record_id:record.id,expected_revision:record.revision,kind:'correct',new_value:'private draft'})
  await runInDurableObject(brain as never,(instance,state)=>{
    const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
    const prefix = 'anda-brain:product:v1:change:'
    const original = state.storage.kv.get<any>(prefix+preview.operation_key)!
    for(let i=0;i<125;i++) {
      const entry = structuredClone(original)
      entry.receipt.expires_at=0
      state.storage.kv.put(prefix+`test-${String(i).padStart(3,'0')}`,entry)
    }
    const a = nexus.execute('CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:"erase one"}}').handles.e!
    const b = nexus.execute('CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:"erase two"}}').handles.e!
    let calls=0
    const product = new MemoryProduct(nexus,state.storage)
    const result=forget(nexus.systemSession(),{entities:[a,b]},ids=>{calls++;expect(ids.size).toBe(2);product.scrubChanges(ids)})
    expect(result.deleted_evidence).toBe(2)
    expect(calls).toBe(1)
    for(const [,entry] of state.storage.kv.list<any>({prefix:prefix+'test-'})) {
      expect(entry.receipt.state).toBe('discarded')
      expect(JSON.stringify(entry)).not.toContain('private draft')
    }
  })
})
