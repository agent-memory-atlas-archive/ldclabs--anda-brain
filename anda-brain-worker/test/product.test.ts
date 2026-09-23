import { env, evictDurableObject, runInDurableObject } from 'cloudflare:test'
import { describe, expect, it } from 'vitest'
import { anonymousAuth, contentDigest, principalAuth, systemAuth, SYSTEM_PRINCIPAL, parseKip, type CognitiveNexus } from '@ldclabs/kip-do'
import { formMemory, recallMemory, maintainMemory } from '../src/operations.js'
import { MemoryProduct, assertCurrentOperations } from '../src/product.js'
import type { AndaBrain } from '../src/brain.js'
import type { BrainRpc, Env, FormationInput } from '../src/types.js'

const auth = systemAuth()
const plan = { types: [], predicates: [], summary: 'stored', commands: [`MUTATE {
  UPSERT CONCEPT ?person {MATCH {type:"Person",key:"${SYSTEM_PRINCIPAL}"} SET FIELDS {name:"owner"}}
  CREATE CONCEPT ?preference {TYPE "Preference" NAME "old preference"}
  ASSERT ?claim (?person,"prefers",?preference) {by:?person,mode:"stated",evidence: :msg1}
}`] }
type Brain = { [K in keyof AndaBrain]: AndaBrain[K] extends (...args: infer A) => infer R ? (...args: A) => Promise<Awaited<R>> : never }
function stub() { return env.BRAIN.getByName(crypto.randomUUID()) as unknown as Brain }
function runtime(responses: unknown[]): Env {
  return { BRAIN:env.BRAIN, AI:{ async run() { const response = responses.shift(); if (response === undefined) throw new Error('unexpected AI call'); return {response} } } } as Env
}
async function seed() {
  const brain = stub()
  const input: FormationInput = {messages:[{role:'user',content:'my old preference'}],context:{source:'chat-42'},timestamp:'2026-09-20T00:00:00Z'}
  await formMemory(runtime([plan]), brain as unknown as BrainRpc, input)
  const page = await brain.productRecords(auth)
  expect(page.records).toHaveLength(1)
  expect(page.complete).toBe(true)
  return {brain, input, record:page.records[0]!}
}

describe('recoverable memory product', () => {
  it('projects claims and independently verifiable host sources across eviction', async () => {
    const {brain, record} = await seed()
    expect(record).toMatchObject({stance:'support',status:'active',actor_key:SYSTEM_PRINCIPAL,sources_complete:true})
    expect(record.sources[0]).toMatchObject({evidence_id:'E-1',message_index:0,product_operation:null})
    expect(record.sources[0]!.payload_digest).toMatch(/^sha256:/)
    await evictDurableObject(brain as never)
    expect(await brain.productRecord(auth,record.id)).toEqual(record)
    expect(await brain.productSource(auth,'E-1')).toEqual(record.sources[0])
    await rejects(brain.productRecords(anonymousAuth()), 'unauthorized')
  })

  it('requires exact reviewed scope, preserves claim history, and binds correction source', async () => {
    const {brain, record, input} = await seed()
    const request = {operation_id:'correction',record_id:record.id,expected_revision:record.revision,kind:'correct' as const,new_value:'new preference'}
    const preview = await brain.productPrepare(auth,request)
    expect(preview.state).toBe('prepared')
    expect((await brain.productRecord(auth,record.id)).status).toBe('active')
    expect(await brain.productPrepare(auth,request)).toEqual(preview)
    await rejects(brain.productPrepare(auth,{...request,new_value:'other'}), 'idempotency_conflict')
    await rejects(brain.productCommit(auth,'correction','wrong'), 'revision_conflict')
    const done = await brain.productCommit(auth,'correction',preview.preview_digest)
    expect(done.state,JSON.stringify(done)).toBe('confirmed')
    const replacement = await brain.productRecord(auth,done.replacement_record!)
    expect(replacement.object_label).toBe('new preference')
    expect((await brain.productRecord(auth,record.id)).status).toBe('retracted')
    expect(await brain.productCorrectionSource(auth,replacement.sources[0]!)).toBe('new preference')
    expect(await brain.productCorrectionSource(auth,{...replacement.sources[0]!,payload_digest:'fake'})).toBeNull()
    expect(await brain.productCommit(auth,'correction',preview.preview_digest)).toEqual(done)
    await rejects(formMemory(runtime([plan]),brain as unknown as BrainRpc,input), 'source_suppressed')
    await rejects(brain.executeFormationPlan(plan.commands.map(command=>({command}))), 'memory_changed')
    await rejects(brain.declareSymbols(['StaleType'],[]), 'memory_changed')
  })

  it('suppresses the full source closure, keeps technical audit, and fences historical agent reads', async () => {
    const {brain, record} = await seed()
    const prepared = await brain.productPrepare(auth,{operation_id:'hide',record_id:record.id,expected_revision:record.revision,kind:'suppress'})
    expect(prepared.preview.targets.map(row=>row.kind).sort()).toEqual(['assertion','evidence','proposition'])
    expect((await brain.productCommit(auth,'hide',prepared.preview_digest)).state).toBe('confirmed')
    const epoch = await brain.beginProcessing()
    expect((await brain.executeAgentRead([{command:'FIND(?a) WHERE {?a ASSERTION {}} LIMIT 20'}],epoch))[0]!.result).toEqual([])
    await rejects(brain.executeAgentRead([{command:'FIND(?a) WHERE {?a ASSERTION {state:"archived"}} LIMIT 20'}],epoch), 'inactive_memory_disabled')
    await rejects(brain.executeAgentRead([{command:'HISTORY ELEMENT "A-1" LIMIT 10'}],epoch), 'historical_memory_disabled')
    expect((await brain.productRecord(auth,record.id)).storage_state).toBe('archived')
    await evictDurableObject(brain as never)
    expect((await brain.productStatus()).epoch).toBe(epoch)
  })

  it('deletes Evidence and Activity, clears related previews, and retains legal holds', async () => {
    const {brain,record} = await seed()
    const activity = await brain.executeKip('CREATE ACTIVITY ?activity {SET FIELDS {activity_class:"encoding",status:"completed"} SET STRUCTURAL {("inputs", :e)}}',{e:record.sources[0]!.evidence_id})
    expect(activity.status,JSON.stringify(activity)).toBe('succeeded')
    const old = await brain.productPrepare(auth,{operation_id:'old',record_id:record.id,expected_revision:record.revision,kind:'correct',new_value:'sensitive draft'})
    const prepared = await brain.productPrepare(auth,{operation_id:'erase',record_id:record.id,expected_revision:record.revision,kind:'delete'})
    expect(prepared.preview.targets.some(row=>row.kind==='activity')).toBe(true)
    const done = await brain.productCommit(auth,'erase',prepared.preview_digest)
    expect(done.state,JSON.stringify(done)).toBe('confirmed')
    expect(done.preview.record.text).toBe('')
    const discarded = await brain.productChange(auth,old.operation_id)
    expect(discarded.state).toBe('discarded')
    expect(JSON.stringify(discarded)).not.toContain('sensitive draft')
    await rejects(brain.productRecord(auth,record.id), 'not_found')
    const evidence = await brain.executeKip('FIND(?e.payload) WHERE {?e EVIDENCE {id:"E-1",state:"purged"}} LIMIT 1')
    expect(JSON.stringify(evidence)).not.toContain('my old preference')
    const held = await seed()
    const hold = await held.brain.executeKip('SET RETENTION :id {legal_hold:true}',{id:held.record.sources[0]!.evidence_id})
    expect(hold.status,JSON.stringify(hold)).toBe('succeeded')
    await rejects(held.brain.productPrepare(auth,{operation_id:'held',record_id:held.record.id,expected_revision:held.record.revision,kind:'delete'}), 'unsupported_scope')
  })

  it('refuses stale previews when the source closure grows and expires discarded previews', async () => {
    const {brain,record} = await seed()
    const prepared = await brain.productPrepare(auth,{operation_id:'erase',record_id:record.id,expected_revision:record.revision,kind:'delete'})
    await brain.executeKip('CREATE ACTIVITY ?x {SET FIELDS {activity_class:"later",status:"completed"} SET STRUCTURAL {("inputs", :e)}}',{e:record.sources[0]!.evidence_id})
    await rejects(brain.productCommit(auth,'erase',prepared.preview_digest), 'revision_conflict')
    await brain.productDiscard(auth,'erase')
    await rejects(brain.productCommit(auth,'erase',prepared.preview_digest), 'preview_expired')
    expect((await brain.productStatus()).epoch).toBe(0)
  })

  it('recovers a committed native correction whose acknowledgement was lost before eviction', async () => {
    const {brain,record} = await seed()
    const preview = await brain.productPrepare(auth,{operation_id:'recover',record_id:record.id,expected_revision:record.revision,kind:'correct',new_value:'recovered preference'})
    await runInDurableObject(brain as never, (instance, state) => {
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const key = 'anda-brain:product:v1:change:' + preview.operation_key
      const stored = state.storage.kv.get<any>(key)!
      state.storage.kv.put('anda-brain:product:v1:control',{version:1,epoch:1,suppressed:preview.preview.excluded_sources,pending:preview.operation_key})
      const command = parseKip(stored.requests[0].command)
      if (!('Kml' in command)) throw new Error('fixture')
      nexus.session(auth).mutate(command.Kml,stored.requests[0].parameters,{idempotencyKey:`memory-product:${preview.operation_key}:correction`})
      // Do not update the journal. This is the native-commit / lost-receipt boundary.
    })
    await evictDurableObject(brain as never)
    const recovered = await brain.productChange(auth,'recover')
    expect(recovered.state,JSON.stringify(recovered)).toBe('confirmed')
    expect((await brain.productRecords(auth)).records).toHaveLength(2)
    expect((await brain.productStatus()).available).toBe(true)
    expect(await brain.productCommit(auth,'recover',preview.preview_digest)).toEqual(recovered)
  })

  it('holds the fence on unresolved writes and resumes on reload without widening the preview', async () => {
    const {brain,record} = await seed()
    const preview = await brain.productPrepare(auth,{operation_id:'pending',record_id:record.id,expected_revision:record.revision,kind:'delete'})
    await runInDurableObject(brain as never, (instance,state) => {
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      state.storage.kv.put('anda-brain:product:v1:control',{version:1,epoch:1,suppressed:preview.preview.excluded_sources,pending:preview.operation_key})
      // A late native hold must be honored even during recovery.
      nexus.execute('SET RETENTION :id {legal_hold:true}',{id:record.id})
    })
    await evictDurableObject(brain as never)
    const pending = await brain.productChange(auth,'pending')
    expect(pending.state).toBe('reconciling')
    expect((await brain.productStatus()).available).toBe(false)
    await rejects(brain.beginProcessing(), 'memory_change_pending')
    expect(pending.preview_digest).toBe(preview.preview_digest)
  })

  it('requires a recipient binding and cancels without rearming a Watch after retries or eviction', async () => {
    const {brain,record} = await seed()
    await rejects(brain.productCreateRecordWatch(auth,'watch',record.id,'notify'), 'recipient_binding_required')
    await runInDurableObject(brain as never,(instance,state)=>{
      const product = new MemoryProduct((instance as unknown as {nexus:CognitiveNexus}).nexus,state.storage,auth.principal_id)
      const watch = product.createWatch(auth,'watch',record.id,'notify')
      expect(watch.state).toBe('armed')
      expect(product.cancelWatch(auth,'watch').state).toBe('cancelled')
      expect(product.createWatch(auth,'watch',record.id,'notify').state).toBe('cancelled')
    })
    await evictDurableObject(brain as never)
    await runInDurableObject(brain as never,(instance,state)=>{
      const product = new MemoryProduct((instance as unknown as {nexus:CognitiveNexus}).nexus,state.storage,auth.principal_id)
      expect(product.createWatch(auth,'watch',record.id,'notify').state).toBe('cancelled')
    })
  })

  it('caps record pages at the current native max_results constraint', async () => {
    const {brain,record} = await seed()
    const second = await brain.executeKip(`MUTATE {
      CREATE CONCEPT ?another {TYPE "Preference" NAME "another preference"}
      ASSERT ?claim (:subject, :predicate, ?another) {by: :actor,mode:"stated"}
    }`,{subject:record.subject,predicate:record.predicate,actor:{id:record.actor_id}})
    expect(second.status,JSON.stringify(second)).toBe('succeeded')
    await runInDurableObject(brain as never,(instance,state)=>{
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const principal = 'kip:principal:limited-reader'
      nexus.store.governance.ensurePrincipal({principal_id:principal})
      nexus.systemSession().createGrant({space_id:nexus.space,grantee_principal:principal,
        actions:['read','discover'],constraints:{max_results:1}})
      const product = new MemoryProduct(nexus,state.storage)
      const page = product.records(principalAuth(principal),undefined,50)
      expect(page.records).toHaveLength(1)
      expect(page.next_cursor).not.toBeNull()
      expect(product.records(principalAuth(principal),page.next_cursor!,50).records).toHaveLength(1)

      const zero = 'kip:principal:zero-results'
      nexus.store.governance.ensurePrincipal({principal_id:zero})
      nexus.systemSession().createGrant({space_id:nexus.space,grantee_principal:zero,
        actions:['read','discover'],constraints:{max_results:0}})
      expect(product.records(principalAuth(zero),undefined,50)).toMatchObject({
        records:[],next_cursor:null,complete:false,
      })
      expect(()=>product.record(principalAuth(zero),record.id)).toThrow('not_found')
      expect(()=>product.source(principalAuth(zero),record.sources[0]!.evidence_id)).toThrow('not_found')
    })
  })

  it('cancels a preparing Watch with or without a lost native creation receipt', async () => {
    const {brain,record} = await seed()
    await runInDurableObject(brain as never,(instance,state)=>{
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const product = new MemoryProduct(nexus,state.storage,auth.principal_id)
      const pending = (operation_id:string,summary:string) => {
        const key = contentDigest({caller:auth.principal_id,operation_id}).slice(7)
        state.storage.kv.put('anda-brain:product:v1:watch:'+key,{
          operation_id,watch_id:'',target_id:record.id,state:'preparing',
          digest:contentDigest({target:record.id,summary}),
        })
      }
      pending('failed-create','failed create')
      expect(product.cancelWatch(auth,'failed-create').state).toBe('cancelled')
      expect(product.createWatch(auth,'failed-create',record.id,'failed create').state).toBe('cancelled')

      product.createWatch(auth,'lost-receipt',record.id,'lost receipt')
      pending('lost-receipt','lost receipt')
      const cancelled = product.cancelWatch(auth,'lost-receipt')
      expect(cancelled.watch_id).toMatch(/^C-/)
      expect(cancelled.state).toBe('cancelled')
      expect(product.createWatch(auth,'lost-receipt',record.id,'lost receipt').state).toBe('cancelled')
    })
  })

  it('resumes a partially committed deletion after eviction', async () => {
    const {brain,record} = await seed()
    const preview = await brain.productPrepare(auth,{operation_id:'partial',record_id:record.id,expected_revision:record.revision,kind:'delete'})
    await runInDurableObject(brain as never,(instance,state)=>{
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const original = nexus.session.bind(nexus)
      let writes = 0
      nexus.session = caller => {
        const session = original(caller)
        const mutate = session.mutate.bind(session)
        session.mutate = (...args) => { if (++writes === 2) throw new Error('simulated interruption'); return mutate(...args) }
        return session
      }
      try {
        const product = new MemoryProduct(nexus,state.storage)
        const partial = product.commit(auth,'partial',preview.preview_digest)
        expect(partial.state).toBe('reconciling')
        expect(product.state().pending).toBe(preview.operation_key)
      } finally { nexus.session = original }
    })
    await evictDurableObject(brain as never)
    expect((await brain.productChange(auth,'partial')).state).toBe('confirmed')
    expect((await brain.productStatus()).available).toBe(true)
  })

  it('rechecks native read authority for saved previews and does not equate another actor with the caller', async () => {
    const {brain,record} = await seed()
    await runInDurableObject(brain as never,(instance,state)=>{
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const principal = 'kip:principal:reviewer'
      nexus.store.governance.ensurePrincipal({principal_id:principal})
      const grant = nexus.systemSession().createGrant({space_id:nexus.space,grantee_principal:principal,actions:['read','discover','archive','moderate_assertion']})
      const caller = principalAuth(principal)
      const product = new MemoryProduct(nexus,state.storage)
      expect(()=>product.prepare(caller,{operation_id:'wrong-actor',record_id:record.id,expected_revision:record.revision,kind:'correct',new_value:'unattributed'})).toThrow('unsupported_scope')
      const preview = product.prepare(caller,{operation_id:'reviewed',record_id:record.id,expected_revision:record.revision,kind:'suppress'})
      expect(preview.state).toBe('prepared')
      nexus.systemSession().revokeGrant(grant.id)
      expect(()=>product.change(caller,'reviewed')).toThrow()
      expect(()=>product.commit(caller,'reviewed',preview.preview_digest)).toThrow()
      expect(product.state().epoch).toBe(0)
    })
  })

  it('advances recipient-owned record watches using native generation and coverage', async () => {
    const {brain,record} = await seed()
    await runInDurableObject(brain as never,(instance,state)=>{
      const nexus = (instance as unknown as {nexus:CognitiveNexus}).nexus
      const product = new MemoryProduct(nexus,state.storage,auth.principal_id)
      product.createWatch(auth,'notify',record.id,'claim changed')
      expect(product.advanceWatch(auth,'notify').state).toBe('armed')
      nexus.execute('TRANSITION :id TO "retracted"',{id:record.id})
      expect(product.advanceWatch(auth,'notify').state).toBe('fired')
      expect(product.createWatch(auth,'notify',record.id,'claim changed').state).toBe('fired')
    })
  })

  it('fences a successful Recall answer if memory changes while the answer model is running', async () => {
    const {brain,record} = await seed()
    let unblock!:()=>void, started!:()=>void, calls = 0
    const waiting = new Promise<void>(resolve=>{started=resolve})
    const resume = new Promise<void>(resolve=>{unblock=resolve})
    const runtimeEnv = {BRAIN:env.BRAIN,AI:{async run(){
      if (++calls === 1) return {response:{commands:[]}}
      started()
      await resume
      return {response:{answer:'old preference',found:true,uncertainty:0.1}}
    }}} as Env
    const running = recallMemory(runtimeEnv,brain as unknown as BrainRpc,{query:'preference'}).catch(error=>error as Error)
    await waiting
    const preview = await brain.productPrepare(auth,{operation_id:'answer-race',record_id:record.id,expected_revision:record.revision,kind:'suppress'})
    await brain.productCommit(auth,'answer-race',preview.preview_digest)
    unblock()
    expect(((await running) as Error).message).toContain('memory_changed')
  })

  it('does not accept a model-authored source key as a host-captured observation', async () => {
    const {brain,record} = await seed()
    const forged = await brain.executeKip(`MUTATE {
      CREATE EVIDENCE ?fake {CLIENT KEY :key SET FIELDS {evidence_class:"user_statement",payload:"fabricated",observed_at:"2026-09-20T00:00:00.000Z"}}
      CREATE CONCEPT ?value {TYPE "Preference" NAME "fabricated preference"}
      ASSERT ?claim (:actor,"prefers",?value) {by: :actor,mode:"stated",evidence:?fake}
    }`,{key:record.sources[0]!.origin+':99',actor:{id:record.actor_id!}})
    expect(forged.status).toBe('succeeded')
    const claim = forged.extensions!['kip-do/outcome']!.handles.claim!
    expect((await brain.productRecord(auth,claim)).sources_complete).toBe(false)
  })

  it.each(['formation','recall','maintenance'] as const)('fences %s completion paused across a managed change', async mode => {
    const {brain,record} = await seed()
    let unblock!:()=>void, started!:()=>void
    const waiting = new Promise<void>(resolve=>{started=resolve})
    const resume = new Promise<void>(resolve=>{unblock=resolve})
    const runtimeEnv: Env = { BRAIN:env.BRAIN, AI:{ async run() {
      started()
      await resume
      return {response:mode==='recall'?{commands:[]}:{...plan,types:['StaleType']}}
    } } } as Env
    const running = mode==='formation' ? formMemory(runtimeEnv,brain as unknown as BrainRpc,{messages:[{role:'user',content:'new input'}]})
      : mode==='maintenance' ? maintainMemory(runtimeEnv,brain as unknown as BrainRpc,{})
      : recallMemory(runtimeEnv,brain as unknown as BrainRpc,{query:'preference'})
    const caught = running.catch(error=>error as Error)
    await waiting
    const current = await brain.productRecord(auth,record.id)
    const preview = await brain.productPrepare(auth,{operation_id:'race',record_id:record.id,expected_revision:current.revision,kind:'suppress'})
    expect((await brain.productCommit(auth,'race',preview.preview_digest)).state).toBe('confirmed')
    unblock()
    expect(((await caught) as Error).message).toContain('memory_changed')
    expect((await brain.vocabulary()).types).not.toContain('StaleType')
  })
})

it('rejects historical reads inside nested AST while retaining ordinary literal payloads',()=>{
  for (const command of [
    'FIND(?e) WHERE {OPTIONAL {?e EVIDENCE {state:"archived"}}} LIMIT 1',
    'FIND(?e) WHERE {?e EVIDENCE {}} AS OF SEQ 1 LIMIT 1',
    'DESCRIBE TRANSACTION "tx-old"',
  ]) expect(()=>assertCurrentOperations([{command}])).toThrow()
  expect(()=>assertCurrentOperations([{command:'CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:{state:"a literal",cursor:"source text",matcher:{state:"not a selector"}}}}'}])).not.toThrow()
})

it('recovers erased preview cleanup even when a retry has no live graph targets', async () => {
  const {brain,record} = await seed()
  const preview = await brain.productPrepare(auth,{operation_id:'cleanup-retry',record_id:record.id,expected_revision:record.revision,kind:'correct',new_value:'private pending correction'})
  await runInDurableObject(brain as never,(instance,state)=>{
    const nexus=(instance as unknown as {nexus:CognitiveNexus}).nexus
    const product=new MemoryProduct(nexus,state.storage)
    product.invalidate()
    const outcome=nexus.execute('PURGE :id REFERENCE POLICY "authorized_cascade" CONFIRM "PURGE"',{id:record.sources[0]!.evidence_id})
    const ids=outcome.changes.filter(change=>change.op==='purge').map(change=>change.id)
    // Native deletion committed; preview cleanup has not completed.
    state.storage.kv.put('anda-brain:product:v1:pending_scrub',{ids})
    expect(()=>product.begin()).toThrow('memory_change_pending')
  })
  await evictDurableObject(brain as never)
  const receipt=await brain.productChange(auth,preview.operation_id)
  expect(receipt.state).toBe('discarded')
  expect(JSON.stringify(receipt)).not.toContain('private pending correction')
  expect(await brain.beginProcessing()).toBe(1)
})

// Await RPC thenables directly; Vitest's rejects matcher probes thenables more
// than once, which produces spurious unhandled rejections in workerd.
async function rejects(result: PromiseLike<unknown>, message: string): Promise<void> {
  let error: unknown
  try { await result } catch (caught) { error = caught }
  finally { (result as PromiseLike<unknown> & { [Symbol.dispose]?: () => void })[Symbol.dispose]?.() }
  expect(error).toBeInstanceOf(Error)
  expect((error as Error).message).toContain(message)
}
