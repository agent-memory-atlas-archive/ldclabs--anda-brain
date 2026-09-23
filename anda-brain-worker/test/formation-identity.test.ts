import { env } from 'cloudflare:test'
import { expect, it } from 'vitest'
import { handleRequest } from '../src/index.js'
import type { AiBinding, BrainRpc, Env } from '../src/types.js'

class FakeAi implements AiBinding {
  constructor(private plans: unknown[]) {}
  async run(): Promise<unknown> { return { response: this.plans.shift() } }
}
function runtime(plans: unknown[]): Env {
  return { ...env, BRAIN_API_KEY: '', AI: new FakeAi(plans) } as unknown as Env
}
function post(e: Env, space: string, action: string, body: unknown): Promise<Response> {
  return handleRequest(new Request(`https://test/v1/${space}/${action}`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  }), e)
}
const plan = {
  types: [], predicates: [], summary: 'stored',
  commands: [`MUTATE {
    UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS {name: "Alice"} }
    CREATE CONCEPT ?preference { TYPE "Preference" NAME "current preference" }
    ASSERT ?a (?alice, "prefers", ?preference) { by: ?alice, mode: "stated", evidence: :msg1 }
  }`],
}

it('preserves distinct messages from the same source thread', async () => {
  const e = runtime([plan, plan]); const space = `review-source-${crypto.randomUUID()}`
  for (const content of ['My old preference', 'My corrected preference']) {
    const r = await post(e, space, 'formation', {
      context: {source: 'same-thread'}, messages: [{role:'user',content}],
      timestamp: '2026-09-07T00:00:00.000Z',
    })
    expect(r.status, await r.text()).toBe(200)
  }
  const r = await post(e, space, 'execute_kip_readonly', {command: 'FIND(?e.payload) WHERE { ?e EVIDENCE {} } LIMIT 10'})
  const body = await r.json() as any
  console.log('same-thread Evidence:', JSON.stringify(body))
  expect(JSON.stringify(body)).toContain('My corrected preference')
})

it('honors memory_strength_decay_factor=1 in deterministic settlement', async () => {
  const e = runtime([{types:[],predicates:[],commands:[],summary:'no changes'}])
  const space = `review-decay-${crypto.randomUUID()}`
  const created = await post(e, space, 'execute_kip', {command: `CREATE CONCEPT ?p {
    TYPE "Preference" NAME "keep accessible" SET FACET "MnemonicState" { memory_strength: 0.8 }
  }`})
  expect(created.status, await created.text()).toBe(200)
  const maintained = await post(e, space, 'maintenance', {parameters:{memory_strength_decay_factor:1}})
  expect(maintained.status, await maintained.text()).toBe(200)
  const read = await post(e, space, 'execute_kip_readonly', {command:'FIND(?c.facets["MnemonicState"].memory_strength) WHERE { ?c CONCEPT {type: "Preference"} } LIMIT 10'})
  const result = await read.json() as any
  console.log('decay factor=1 result:', JSON.stringify(result))
  expect(result.result[0].result[0]).toBe(0.8)
})

it('discovers every correction when one transaction exceeds a page', async () => {
  const brain = env.BRAIN.getByName(`correction-pages-${crypto.randomUUID()}`) as unknown as BrainRpc
  const assertions = Array.from({length: 21}, (_,i) =>
    `ASSERT ?old${i} (?actor, "prefers", ?preference) {by: ?actor, mode: "stated"}`)
  const [seed] = await brain.executeKipBatch([{command: `MUTATE {
    CREATE CONCEPT ?actor {TYPE "Person" NAME "Actor"}
    CREATE CONCEPT ?preference {TYPE "Preference" NAME "Preference"}
    ${assertions.join('\n')}
    ASSERT ?replacement (?actor, "prefers", ?preference) {by: ?actor, mode: "stated"}
  }`}])
  expect(seed?.status, JSON.stringify(seed)).toBe('succeeded')
  const handles = seed?.extensions?.['kip-do/outcome']?.handles
  if (!handles) throw new Error('missing fixture handles')
  const parameters = handles
  const [changed] = await brain.executeKipBatch([{command: `MUTATE {
    ${assertions.map((_,i) => `TRANSITION :old${i} TO "superseded" BY :replacement`).join('\n')}
  }`, parameters}])
  expect(changed?.status, JSON.stringify(changed)).toBe('succeeded')
  const first = await brain.settleMemory(Date.now())
  expect(first.corrections.revised_roots).toHaveLength(20)
  expect(first.corrections.incomplete).toBe(true)
  expect(first.corrections.cursor_after_id).toBeDefined()
  await brain.acknowledgeCorrections(first.corrections.revised_roots.map(root => root.assertion), 0)
  const second = await brain.settleMemory(Date.now())
  expect(second.corrections.revised_roots).toHaveLength(1)
  expect(second.corrections.incomplete).toBe(false)
  const ids = [...first.corrections.revised_roots,...second.corrections.revised_roots].map((r) => r.assertion)
  expect(new Set(ids).size).toBe(21)
  await brain.acknowledgeCorrections(second.corrections.revised_roots.map(root => root.assertion), 0)
  expect((await brain.settleMemory(Date.now())).corrections.revised_roots).toHaveLength(0)
})
