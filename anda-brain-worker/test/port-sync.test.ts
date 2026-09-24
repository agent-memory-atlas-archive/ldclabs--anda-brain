import { env } from 'cloudflare:test'
import { expect, it } from 'vitest'
import { KIP_SYNTAX } from '../src/assets.generated.js'
import { handleRequest } from '../src/index.js'
import { formationMessages, formationReviewMessages, recallPlanMessages, recallAnswerMessages, maintenanceMessages } from '../src/prompts.js'
import type { AiBinding, Env } from '../src/types.js'

class Ai implements AiBinding {
  calls: {role:string;content:string}[][] = []
  constructor(private replies: unknown[]) {}
  async run(_model:string,input:Record<string,unknown>) {
    this.calls.push(input.messages as {role:string;content:string}[])
    const response = this.replies.shift()
    if (response instanceof Error) throw response
    if (response === undefined) throw new Error('unexpected AI call')
    return {response,usage:{input_tokens:10,output_tokens:5}}
  }
}
const empty = {types:[],predicates:[],commands:[],summary:'no additional changes'}
const plan = {...empty, types:['AnswerStyle'], commands:[`MUTATE {
  CREATE CONCEPT ?actor {TYPE "Person" NAME "Alice"}
  CREATE CONCEPT ?value {TYPE "AnswerStyle" NAME "concise"}
  ASSERT ?claim (?actor,"prefers",?value) {by:?actor,mode:"stated",evidence: :msg1,at:"2026-09-22T00:00:00.000Z"}
}`],summary:'stored'}
function post(ai: Ai, space: string, action: string, body: unknown) {
  return handleRequest(new Request(`https://test/v1/${space}/${action}`,{method:'POST',body:JSON.stringify(body)}),{BRAIN:env.BRAIN,AI:ai} as Env)
}
it('loads the exact full syntax in every stage including review and answering',()=>{
  const input = {messages:[{role:'user' as const,content:'remember'}]}
  for (const messages of [formationMessages({},input,''),formationReviewMessages({},input,'',[]),
    recallPlanMessages({},{query:'query'}),recallAnswerMessages({query:'query'},[]),maintenanceMessages({},[], '')]) {
    expect(messages[0]!.content).toContain(KIP_SYNTAX)
  }
})
it('performs exactly one large-input review with receipts, captured window and summed usage',async()=>{
  const ai = new Ai([plan,{...empty,references:[{document:'syntax',section:'kml',offset:0}],summary:''},empty])
  const space = crypto.randomUUID()
  const response = await post(ai,space,'formation',{messages:Array.from({length:20},(_,i)=>({role:'user',content:`message-${i+1} `+'x'.repeat(2100)}))})
  const body = await response.json<any>()
  expect(response.status,JSON.stringify(body)).toBe(200)
  expect(body.result.review).toMatchObject({performed:true,commands:0})
  expect(body.result.usage).toEqual({input_tokens:30,output_tokens:15})
  expect(ai.calls).toHaveLength(3)
  const review = JSON.parse(ai.calls[1]![1]!.content)
  expect(review.captured_window).toMatchObject({first_message:5,last_message:20,bindings:'msg1 through msg16',write_time_only:true})
  expect(JSON.stringify(review.receipts)).toContain('E-1')
  expect(review.receipts[0].status).toBe('succeeded')
  const evidence = await (await post(new Ai([]),space,'execute_kip_readonly',{command:'FIND(?e.id) WHERE {?e EVIDENCE {}} LIMIT 20'})).json<any>()
  expect(evidence.result[0].result).toHaveLength(16)
})
it.each([new Error('model failed'),{...empty,commands:['PURGE "A-1" CONFIRM "PURGE"']},
  {...empty,commands:['CREATE CONCEPT ?x {TYPE "UnknownType"}']}])('retains initial receipts when review fails',async repair=>{
  const ai = new Ai([plan,repair]), space = crypto.randomUUID()
  const response = await post(ai,space,'formation',{messages:[{role:'user',content:'x'.repeat(40_000)}]})
  const body = await response.json<any>()
  expect(response.status,JSON.stringify(body)).toBe(422)
  expect(body.error.message).toContain('inspect receipts')
  expect(body.error.data.initial_results[0].status).toBe('succeeded')
  const assertions = await (await post(new Ai([]),space,'execute_kip_readonly',{command:'FIND(?a.id) WHERE {?a ASSERTION {}} LIMIT 20'})).json<any>()
  expect(assertions.result[0].result).toHaveLength(1)
})
it('does not review a small input or a failed initial write',async()=>{
  const small = new Ai([empty])
  expect((await post(small,crypto.randomUUID(),'formation',{messages:[{role:'user',content:'small'}]})).status).toBe(200)
  expect(small.calls).toHaveLength(1)
  const failed = new Ai([{...empty,commands:['CREATE CONCEPT ?x {TYPE "MissingType"}']}])
  expect((await post(failed,crypto.randomUUID(),'formation',{messages:[{role:'user',content:'x'.repeat(40_000)}]})).status).toBe(422)
  expect(failed.calls).toHaveLength(1)
})
it('grounds the actual question before planning and refuses unsupported budgets explicitly',async()=>{
  const ai = new Ai([{commands:[]},{answer:'unknown',found:false,uncertainty:1}])
  expect((await post(ai,crypto.randomUUID(),'recall_structured',{query:'question'})).status).toBe(200)
  expect(JSON.parse(ai.calls[0]![1]!.content)).toMatchObject({query:'question',grounding:{status:'succeeded'}})
  const unsupported = new Ai([])
  const response = await post(unsupported,crypto.randomUUID(),'recall_structured',{query:'question',budget:{max_output_tokens:100}})
  expect(response.status).toBe(400)
  expect(await response.text()).toContain('budgeted Recall is not supported')
  expect(unsupported.calls).toHaveLength(0)
})
it('forgets Evidence and Activity with dry run, legal holds and exact per-kind counts',async()=>{
  const ai = new Ai([]), space = crypto.randomUUID()
  const created = await (await post(ai,space,'execute_kip',{command:`MUTATE {
    CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:"erase me"}}
    CREATE ACTIVITY ?x {SET FIELDS {activity_class:"test",status:"completed"}}
    CREATE EVIDENCE ?held {SET FIELDS {evidence_class:"user_statement",payload:"hold me"}}
    SET RETENTION ?held {legal_hold:true}
  }`})).json<any>()
  expect(created.result[0].status,JSON.stringify(created)).toBe('succeeded')
  const ids = created.result[0].extensions['kip-do/outcome'].handles
  const dry = await (await post(ai,space,'memory/forget',{entities:[ids.e,ids.x],dry_run:true})).json<any>()
  expect(dry.result).toMatchObject({deleted_evidence:0,deleted_activities:0,dry_run:true})
  expect(dry.result.entities.every((row:any)=>row.existed)).toBe(true)
  const erased = await (await post(ai,space,'memory/forget',{entities:[ids.e,ids.e,ids.x,ids.held,'bad','E-99999']})).json<any>()
  expect(erased.result).toMatchObject({deleted_evidence:1,deleted_activities:1,deleted_assertions:0})
  expect(erased.result.entities.find((row:any)=>row.entity===ids.held).error).toBeDefined()
  expect(erased.result.entities.find((row:any)=>row.entity==='E-99999')).toEqual({entity:'E-99999',existed:false})
  expect(erased.result.entities).toHaveLength(5)
})

it('forgets archived Evidence and Activity instead of reporting them absent',async()=>{
  const ai = new Ai([]), space = crypto.randomUUID()
  const created = await (await post(ai,space,'execute_kip',{command:`MUTATE {
    CREATE EVIDENCE ?e {SET FIELDS {evidence_class:"user_statement",payload:"archived private text"}}
    CREATE ACTIVITY ?x {SET FIELDS {activity_class:"archived private activity",status:"completed"}}
  }`})).json<any>()
  const ids = created.result[0].extensions['kip-do/outcome'].handles
  for (const id of [ids.e,ids.x]) {
    const transition = await (await post(ai,space,'execute_kip',{
      command:'TRANSITION :id TO "archived"',parameters:{id},
    })).json<any>()
    expect(transition.result[0].status,JSON.stringify(transition)).toBe('succeeded')
  }
  const dry = await (await post(ai,space,'memory/forget',{entities:[ids.e,ids.x],dry_run:true})).json<any>()
  expect(dry.result.entities.every((row:any)=>row.existed)).toBe(true)
  const erased = await (await post(ai,space,'memory/forget',{entities:[ids.e,ids.x]})).json<any>()
  expect(erased.result).toMatchObject({deleted_evidence:1,deleted_activities:1})
  expect(erased.result.entities.every((row:any)=>row.existed && !row.error)).toBe(true)
})
