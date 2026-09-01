import { env, evictDurableObject } from 'cloudflare:test'
import { describe, expect, it } from 'vitest'
import { handleRequest } from '../src/index.js'
import { boundedJson, formationMessages, maintenanceMessages } from '../src/prompts.js'
import type { AiBinding, BrainRpc, Env } from '../src/types.js'

class FakeAi implements AiBinding {
  /** Every `messages` array this binding was handed, newest last. */
  readonly calls: { role: string; content: string }[][] = []

  constructor(private readonly responses: unknown[]) {}

  async run(_model: string, input: Record<string, unknown>): Promise<unknown> {
    if (Array.isArray(input?.messages)) {
      this.calls.push(input.messages as { role: string; content: string }[])
    }
    const response = this.responses.shift()
    if (response === undefined) throw new Error('unexpected AI call')
    if (response instanceof Error) throw response
    return {
      response,
      usage: { input_tokens: 10, output_tokens: 5 },
    }
  }
}

/** The user payload of the last completion — what the model actually saw. */
function lastUserPayload(runtime: Env): string {
  const ai = runtime.AI as FakeAi
  const messages = ai.calls[ai.calls.length - 1] ?? []
  return messages.find((message) => message.role === 'user')?.content ?? '{}'
}

/** One coherent formation: Evidence, the Concepts, the Proposition, the claim. */
const FORMATION_PLAN = `MUTATE {
  UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS { name: "Alice" } }
  CREATE CONCEPT ?concise {
    TYPE "Preference"
    NAME "Alice concise answers"
    SET ATTRIBUTES { preference_class: "communication" }
    SET FACET "MnemonicState" { memory_strength: 0.8, salience: 0.6 }
  }
  CREATE EVIDENCE ?e {
    CLIENT KEY "chat-42:1"
    SET FIELDS {
      evidence_class: "user_statement",
      payload: {source: "chat-42", text: "Please keep your answers concise."},
      observed_at: "2026-08-20T00:00:00Z"
    }
    SET STRUCTURAL { ("source", ?alice) }
  }
  ENSURE PROPOSITION ?p (?alice, "prefers", ?concise)
  CREATE ASSERTION ?a {
    SET FIELDS {
      proposition: ?p,
      asserted_by: ?alice,
      stance: "support",
      mode: "stated",
      confidence: 0.95
    }
  }
}`

describe('Anda Brain Worker', () => {
  it('forms durable memory and retrieves it with read-only KIP', async () => {
    const space = uniqueSpace('formation')
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [FORMATION_PLAN],
          summary: 'Stored Alice’s response-style preference.',
        },
      ]),
    )

    const formed = await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'Please keep your answers concise.' }],
      context: { counterparty: 'alice' },
      timestamp: '2026-08-20T00:00:00Z',
    })
    expect(await text(formed)).toBe('')
    const formedBody = await formed.json<Record<string, any>>()
    // A Proposition and the Assertion that takes a stance on it are two
    // elements in 2.0, and counting them as one would hide the distinction the
    // whole model rests on.
    expect(formedBody.result.stored).toMatchObject({
      concepts: 2,
      propositions: 1,
      assertions: 1,
    })

    const recalled = await post(runtime, space, 'execute_kip_readonly', {
      command:
        'FIND(?c.name, ?c.attributes) WHERE { ?c CONCEPT {type: "Preference"} } LIMIT 5',
    })
    expect(recalled.status).toBe(200)
    expect(JSON.stringify(await recalled.json())).toContain('Alice concise answers')
  })

  it('publishes a symbol the Profile does not have, then writes with it', async () => {
    const space = uniqueSpace('vocabulary')
    const runtime = testEnv(
      new FakeAi([
        {
          types: ['Project'],
          // `drug` is malformed as a type and `Treats` as a predicate; both come
          // back refused rather than published under a tidied-up name.
          predicates: ['works_on', 'Treats'],
          commands: [
            `MUTATE {
              UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS { name: "Alice" } }
              CREATE CONCEPT ?aurora { TYPE "Project" NAME "Aurora" }
              ENSURE PROPOSITION ?p (?alice, "works_on", ?aurora)
            }`,
          ],
          summary: 'Recorded that Alice works on Aurora.',
        },
      ]),
    )

    const formed = await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'I am working on Project Aurora.' }],
    })
    expect(await text(formed)).toBe('')
    const body = await formed.json<Record<string, any>>()
    expect(body.result.vocabulary).toMatchObject({
      package_ref: 'kip://anda-brain/memory@1.0.1',
      types: ['Project'],
      predicates: ['works_on'],
      rejected: ['Treats'],
    })
    expect(body.result.stored.propositions).toBe(1)

    // The package is in force, not merely installed: the Space resolves the
    // local name it published.
    const listed = await get(runtime, space, 'vocabulary')
    expect(await listed.json<Record<string, any>>()).toMatchObject({
      result: { types: ['Project'], predicates: ['works_on'] },
    })
  })

  it('keeps its vocabulary in force across an eviction', async () => {
    const space = uniqueSpace('vocabulary-restart')
    const runtime = testEnv(
      new FakeAi([
        {
          types: ['Project'],
          predicates: [],
          commands: [],
          summary: 'Declared a type.',
        },
      ]),
    )
    await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'I am working on Project Aurora.' }],
    })

    // The object is rebuilt from storage, which runs the constructor again.
    // Activating only the Profile there would *narrow* the environment: the
    // lock would lose this Space's package and `Project` would stop resolving,
    // which is a schema change nobody asked for and no error would name.
    await evictDurableObject(env.BRAIN.getByName(space))

    const written = await post(runtime, space, 'execute_kip', {
      command: 'CREATE CONCEPT ?c { TYPE "Project" NAME "Aurora" }',
    })
    const results = await written.json<Record<string, any>>()
    expect(results.result[0].error).toBeUndefined()
    expect(results.result[0].status).toBe('succeeded')
    expect(results.result[0].extensions['kip-do/outcome'].status).toBe('committed')

    const info = await (await get(runtime, space, 'info')).json<Record<string, any>>()
    // Two activations: the Profile alone on first construction, then the
    // vocabulary beside it. The eviction is not a third.
    expect(info.result.schema_environment_version).toBe(2)
  })

  it('never re-declares a symbol the Profile already provides', async () => {
    const space = uniqueSpace('ambiguity')
    const runtime = testEnv(
      new FakeAi([
        {
          types: ['Person'],
          predicates: ['prefers'],
          commands: [],
          summary: 'Nothing new.',
        },
      ]),
    )
    const formed = await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'Hello.' }],
    })
    const body = await formed.json<Record<string, any>>()
    // Two active packages declaring one local name make every bare
    // `{type: "Person"}` ambiguous, so these are usable but not ours to publish.
    expect(body.result.vocabulary).toMatchObject({
      package_ref: 'kip://anda-brain/memory@1.0.0',
      types: [],
      predicates: [],
      rejected: [],
    })

    const usable = await post(runtime, space, 'execute_kip_readonly', {
      command: 'FIND(COUNT(?c)) WHERE { ?c CONCEPT {type: "Person"} } LIMIT 1',
    })
    expect(usable.status).toBe(200)
    const results = await usable.json<Record<string, any>>()
    expect(results.result[0].error).toBeUndefined()
  })

  it('versions the vocabulary forward and keeps what it already published', async () => {
    const space = uniqueSpace('vocabulary-version')
    const runtime = testEnv(
      new FakeAi([
        { types: ['Project'], predicates: [], commands: [], summary: 'One.' },
        { types: ['Milestone'], predicates: [], commands: [], summary: 'Two.' },
        { types: ['Project'], predicates: [], commands: [], summary: 'Again.' },
      ]),
    )
    const declare = async (): Promise<Record<string, any>> => {
      const response = await post(runtime, space, 'formation', {
        messages: [{ role: 'user', content: 'Remember this.' }],
      })
      return (await response.json<Record<string, any>>()).result.vocabulary
    }

    expect(await declare()).toMatchObject({ package_ref: 'kip://anda-brain/memory@1.0.1' })
    expect(await declare()).toMatchObject({
      package_ref: 'kip://anda-brain/memory@1.0.2',
      types: ['Milestone', 'Project'],
    })
    // Re-declaring what is already there mints no version: every activation
    // mints an environment version that transactions record, and a restart or
    // a repeat is not a schema change.
    expect(await declare()).toMatchObject({ package_ref: 'kip://anda-brain/memory@1.0.2' })
  })

  it('refuses a plan before it publishes anything the plan asked for', async () => {
    const space = uniqueSpace('gate-before-publish')
    const runtime = testEnv(
      new FakeAi([
        {
          types: ['Project'],
          predicates: [],
          commands: ['PURGE "C-1" CONFIRM "PURGE"'],
          summary: 'unsafe plan',
        },
      ]),
    )
    const response = await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'Remember this.' }],
    })
    expect(response.status).toBe(422)

    // A published symbol cannot be tidied away: a schema version, and part of
    // this Space's symbol cap, spent on a word no command was ever allowed to
    // write would be permanent.
    const listed = await (await get(runtime, space, 'vocabulary')).json<Record<string, any>>()
    expect(listed.result).toMatchObject({
      package_ref: 'kip://anda-brain/memory@1.0.0',
      types: [],
    })
  })

  it('lets formation correct a claim by superseding it', async () => {
    const space = uniqueSpace('correction')
    const runtime = testEnv(new FakeAi([]))
    const setup = await post(runtime, space, 'execute_kip', {
      command: `MUTATE {
        UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} }
        CREATE CONCEPT ?dark { TYPE "Preference" NAME "Dark mode" }
        ENSURE PROPOSITION ?p (?alice, "prefers", ?dark)
        CREATE ASSERTION ?a {
          SET FIELDS { proposition: ?p, asserted_by: ?alice, stance: "support", mode: "stated" }
        }
      }`,
    })
    const handles = outcomeOf(await setup.json<Record<string, any>>()).handles

    const plan = `MUTATE {
      CREATE ASSERTION ?b {
        SET FIELDS { proposition: :p, asserted_by: :who, stance: "reject", mode: "stated" }
      }
    }`
    const formation = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [plan, `SUPERSEDE ASSERTION :old BY :new`],
          summary: 'Alice no longer prefers dark mode.',
        },
      ]),
    )
    // The second command needs the id the first minted, which one completion
    // cannot know — so this half is the runtime's, and the test only pins that
    // the gate lets a correction through at all.
    const correction = await post(formation, space, 'execute_kip', {
      operations: [
        { command: plan, parameters: { p: handles.p, who: { id: handles.alice } } },
      ],
    })
    const created = (await correction.json<Record<string, any>>()).result[0]
    expect(created.error).toBeUndefined()

    const superseded = await post(runtime, space, 'execute_kip', {
      command: 'SUPERSEDE ASSERTION :old BY :new',
      parameters: { old: handles.a, new: created.extensions['kip-do/outcome'].handles.b },
    })
    const receipt = (await superseded.json<Record<string, any>>()).result[0]
    expect(receipt.error).toBeUndefined()
    // Nothing was rewritten: the original Assertion is still there, marked.
    const read = await post(runtime, space, 'execute_kip_readonly', {
      command:
        'FIND(?a.id, ?a.lifecycle.status) WHERE { ?a ASSERTION {} } ORDER BY ?a.id LIMIT 5',
    })
    expect(JSON.stringify(await read.json())).toContain('superseded')
  })

  it('grounds recall on a deterministic lookup and cites what it read', async () => {
    const space = uniqueSpace('recall')
    const runtime = testEnv(
      new FakeAi([
        // A planned SEARCH is a read like any other: it has to carry a LIMIT
        // to survive the gate, and it runs beside the grounding lookup.
        { commands: ['SEARCH COGNITION "Alice" LIMIT 5'] },
        { answer: 'Alice prefers concise answers.', found: true, uncertainty: 0.05 },
      ]),
    )

    await post(runtime, space, 'execute_kip', {
      command:
        'CREATE CONCEPT ?c { TYPE "Preference" NAME "Alice concise answers" }',
    })

    const response = await post(runtime, space, 'recall_structured', {
      query: 'How should I answer Alice?',
      context: { counterparty: 'alice' },
    })
    expect(response.status).toBe(200)
    const body = await response.json<Record<string, any>>()
    expect(body.result.answer).toBe('Alice prefers concise answers.')
    expect(body.result.found).toBe(true)
    // The lookup projects id, name and type, so a citation is usable rather
    // than an opaque id.
    expect(body.result.memories[0]).toMatchObject({
      name: 'Alice concise answers',
      type: 'Preference',
    })
    expect(body.result.memories[0].entity).toMatch(/^C-\d+$/)
    expect(body.result.diagnostics.kip_commands).toBe(2)
  })

  it('discards unbounded and mutating model plans, keeping the grounding read', async () => {
    const runtime = testEnv(
      new FakeAi([
        {
          commands: [
            'FIND(?person) WHERE { ?person CONCEPT {type: "Person"} }',
            'SEARCH CONCEPT "person"',
            'MUTATE { CREATE CONCEPT ?x { TYPE "Person" NAME "Mallory" } }',
          ],
        },
        { answer: 'No matching memory.', found: false, uncertainty: 1 },
      ]),
    )

    const response = await post(runtime, uniqueSpace('bounded-recall'), 'recall', {
      query: 'What do I know about this person?',
    })
    expect(response.status).toBe(200)
    const body = await response.json<Record<string, any>>()
    expect(body.result.diagnostics.kip_commands).toBe(1)
    expect(body.result.found).toBe(false)
  })

  it('surfaces a grounding failure instead of reporting a false miss', async () => {
    const runtime = {
      ...testEnv(new FakeAi([{ commands: [] }])),
      BRAIN: { getByName: () => failingReadBrain() } as unknown as Env['BRAIN'],
    }
    const space = uniqueSpace('retrieval-error')

    const probe = await post(runtime, space, 'probe', { query: 'anything' })
    expect(probe.status).toBe(422)
    expect(await probe.text()).toContain('probe KIP failed')

    const recall = await post(runtime, space, 'recall', { query: 'anything' })
    expect(recall.status).toBe(422)
    expect(await recall.text()).toContain('recall KIP failed')
  })

  it('grounds a probe on SEARCH without calling a model', async () => {
    const space = uniqueSpace('probe')
    const runtime = testEnv(new FakeAi([]))
    await post(runtime, space, 'execute_kip', {
      command: 'CREATE CONCEPT ?c { TYPE "Person" NAME :name }',
      parameters: { name: 'Aurora' },
    })

    const hit = await post(runtime, space, 'probe', { query: 'how is aurora going?' })
    const hitBody = await hit.json<Record<string, any>>()
    expect(hitBody.result.found).toBe(true)
    expect(hitBody.result.memories[0].name).toBe('Aurora')

    const miss = await post(runtime, space, 'probe', { query: 'unrelated question' })
    expect((await miss.json<Record<string, any>>()).result.found).toBe(false)
  })

  it('grounds a Chinese question on a Chinese memory', async () => {
    const space = uniqueSpace('cjk')
    const runtime = testEnv(new FakeAi([]))
    await post(runtime, space, 'execute_kip', {
      command: 'CREATE CONCEPT ?c { TYPE "Preference" NAME :name }',
      parameters: { name: '深色模式' },
    })

    // `unicode61` alone would index the whole Han run as one term and answer
    // nothing here; the engine segments both paths with the same function.
    const hit = await post(runtime, space, 'probe', {
      query: '我以后想用什么显示模式？',
    })
    const body = await hit.json<Record<string, any>>()
    expect(body.result.found).toBe(true)
    expect(body.result.memories[0]).toMatchObject({
      name: '深色模式',
      type: 'Preference',
    })
  })

  it('rejects mutations through the read-only endpoint', async () => {
    const runtime = testEnv(new FakeAi([]))
    const response = await post(runtime, uniqueSpace('readonly'), 'execute_kip_readonly', {
      command: 'MUTATE { CREATE CONCEPT ?p { TYPE "Person" NAME "Mallory" } }',
    })
    expect(response.status).toBe(400)
    expect(await response.text()).toContain('read-only KIP')
  })

  it('refuses an atomic batch rather than running it as a sequence', async () => {
    const runtime = testEnv(new FakeAi([]))
    const response = await post(runtime, uniqueSpace('atomic'), 'execute_kip', {
      operations: [
        { command: 'CREATE CONCEPT ?a { TYPE "Person" NAME "A" }' },
        { command: 'CREATE CONCEPT ?b { TYPE "Person" NAME "B" }' },
      ],
      execution: { mode: 'atomic' },
    })
    expect(response.status).toBe(400)
    expect(await response.text()).toContain('atomic')
  })

  it('stops a sequence at the first failure and skips the rest', async () => {
    const space = uniqueSpace('sequence')
    const runtime = testEnv(new FakeAi([]))
    const response = await post(runtime, space, 'execute_kip', {
      operations: [
        { op_id: 'first', command: 'CREATE CONCEPT ?a { TYPE "Person" NAME "A" }' },
        { op_id: 'bad', command: 'CREATE CONCEPT ?b { TYPE "Nonesuch" NAME "B" }' },
        { op_id: 'never', command: 'CREATE CONCEPT ?c { TYPE "Person" NAME "C" }' },
      ],
      execution: { mode: 'sequence', on_error: 'stop' },
    })
    expect(response.status).toBe(200)
    const results = (await response.json<Record<string, any>>()).result
    // The declaration has to reach the engine. An override that dropped it
    // would run this batch as `independent`, and the third Person would be in
    // the graph — committed by a request that asked for it to be skipped.
    expect(results.map((r: any) => [r.op_id, r.status])).toEqual([
      ['first', 'succeeded'],
      ['bad', 'failed'],
      ['never', 'skipped'],
    ])

    const read = await post(runtime, space, 'execute_kip_readonly', {
      command: 'FIND(?c.name) WHERE { ?c CONCEPT {type: "Person"} } LIMIT 5',
    })
    const names = JSON.stringify(await read.json())
    expect(names).toContain('"A"')
    expect(names).not.toContain('"C"')
  })

  it('refuses an op_id that names two operations', async () => {
    const runtime = testEnv(new FakeAi([]))
    const response = await post(runtime, uniqueSpace('opid'), 'execute_kip', {
      operations: [
        { op_id: 'same', command: 'CREATE CONCEPT ?a { TYPE "Person" NAME "A" }' },
        { op_id: 'same', command: 'CREATE CONCEPT ?b { TYPE "Person" NAME "B" }' },
      ],
    })
    expect(response.status).toBe(400)
    expect(await response.text()).toContain('op_id')
  })

  it('reports a mutation that changed nothing as no_effect, not as success', async () => {
    const space = uniqueSpace('noeffect')
    const runtime = testEnv(new FakeAi([]))
    const body = {
      command: 'ARCHIVE ?c WHERE { ?c CONCEPT {type: "Person", key: "nobody"} } LIMIT 1',
    }
    const response = await post(runtime, space, 'execute_kip', body)
    expect(response.status).toBe(200)
    const result = (await response.json<Record<string, any>>()).result[0]
    expect(result.error).toBeUndefined()
    expect(result.status).toBe('no_effect')
  })

  it('rejects a formation plan that reaches for administrative KML', async () => {
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: ['PURGE "C-1" CONFIRM "PURGE"'],
          summary: 'unsafe plan',
        },
      ]),
    )
    const response = await post(runtime, uniqueSpace('formation-guard'), 'formation', {
      messages: [{ role: 'user', content: 'Remember that I like tea.' }],
    })
    expect(response.status).toBe(422)
    expect(await response.text()).toContain('formation cannot issue PURGE')
  })

  it('rejects a formation plan that tries to read instead of write', async () => {
    const runtime = testEnv(
      new FakeAi([
        { types: [], predicates: [], commands: ['DESCRIBE PRIMER'], summary: 'unsafe' },
      ]),
    )
    const response = await post(runtime, uniqueSpace('formation-read'), 'formation', {
      messages: [{ role: 'user', content: 'Remember that I like tea.' }],
    })
    expect(response.status).toBe(422)
    expect(await response.text()).toContain('formation accepts only KIP KML')
  })

  it('runs a no-op maintenance cycle against a real graph snapshot', async () => {
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [],
          summary: 'The graph needs no maintenance.',
        },
      ]),
    )
    const response = await post(runtime, uniqueSpace('maintenance'), 'maintenance', {
      trigger: 'on_demand',
      scope: 'quick',
    })
    expect(await text(response)).toBe('')
    const body = await response.json<Record<string, any>>()
    expect(body.result.scope).toBe('quick')
    expect(body.result.commands).toBe(0)
  })

  it('rejects an unbounded maintenance selection, whichever verb it wears', async () => {
    for (const command of [
      'UPDATE ?event SET ATTRIBUTES { status: "archived" } WHERE { ?event CONCEPT {type: "Event"} }',
      'ARCHIVE ?event WHERE { ?event CONCEPT {type: "Event"} }',
    ]) {
      const runtime = testEnv(
        new FakeAi([
          { types: [], predicates: [], commands: [command], summary: 'Unsafe bulk change.' },
        ]),
      )
      const response = await post(
        runtime,
        uniqueSpace('maintenance-limit'),
        'maintenance',
        { scope: 'full' },
      )
      expect(response.status, command).toBe(422)
      expect(await response.text()).toContain('LIMIT 20')
    }
  })

  it('refuses to let maintenance erase anything', async () => {
    // Both instruments, not just the loud one. `PURGE PAYLOAD` leaves the
    // Evidence record and its citations standing and destroys the bytes
    // underneath them — a narrower blast radius, not a reversible one.
    for (const command of [
      'PURGE "C-1" CONFIRM "PURGE"',
      'PURGE PAYLOAD "E-1" CONFIRM "PURGE"',
    ]) {
      const runtime = testEnv(
        new FakeAi([
          { types: [], predicates: [], commands: [command], summary: 'Cleaning up.' },
        ]),
      )
      const response = await post(
        runtime,
        uniqueSpace('maintenance-purge'),
        'maintenance',
        { scope: 'full' },
      )
      expect(response.status, command).toBe(422)
      // The gate's own message, not merely something containing "PURGE": a
      // parse failure would otherwise let this pass without the gate running.
      expect(await response.text()).toContain('maintenance plans cannot issue KIP PURGE')
    }
  })

  it('fires a silence Watch whose deadline passed, and only that one', async () => {
    // The half of Watch evaluation that is arithmetic. Until the runtime did
    // it, a Commitment whose trigger was a silence Watch waited forever,
    // because nothing in the system noticed a date passing.
    // The settlement runs before the completion, so the cycle still needs one.
    const runtime = testEnv(new FakeAi([{ types: [], predicates: [], commands: [], summary: 'Nothing to do.' }]))
    const space = uniqueSpace('watch-expiry')
    const past = new Date(Date.now() - 86_400_000).toISOString()
    const future = new Date(Date.now() + 86_400_000).toISOString()

    for (const [key, watchClass, due] of [
      ['overdue', 'silence', past],
      ['not_yet', 'silence', future],
      // A delta Watch waits for a matching change; only the model can say
      // what matched, so a deadline means nothing to it.
      ['delta', 'delta', past],
    ]) {
      const created = await post(runtime, space, 'execute_kip', {
        command: `MUTATE {
  UPSERT CONCEPT ?w {
    MATCH { type: "Watch", key: :key }
    SET FIELDS { name: :key }
    SET ATTRIBUTES {
      watch_class: :class, summary: "escalate if nothing lands",
      condition: "no reply from the vendor", status: "armed", due_at: :due
    }
  }
}`,
        parameters: { key, class: watchClass, due },
      })
      expect(created.status, await text(created)).toBe(200)
    }

    const settled = await post(runtime, space, 'maintenance', { scope: 'full' })
    expect(settled.status, await text(settled)).toBe(200)
    const body = (await settled.json()) as { result: { settlement: any } }
    expect(body.result.settlement.watches).toMatchObject({ fired: 1, conflicted: 0 })

    const statuses = await post(runtime, space, 'execute_kip_readonly', {
      command:
        'FIND(?w.name, ?w.attributes.status) WHERE { ?w CONCEPT {type: "Watch"} } LIMIT 10',
    })
    const rows = ((await statuses.json()) as any).result[0].result as [string, string][]
    expect(Object.fromEntries(rows)).toEqual({
      overdue: 'fired',
      not_yet: 'armed',
      delta: 'armed',
    })

    // Firing produced attention and nothing else. An `action_gate` here would
    // be the runtime inventing a decision — act, ask, defer and silence are
    // all judgements about what the deadline means.
    const activities = await post(runtime, space, 'execute_kip_readonly', {
      command: 'FIND(?a.activity_class) WHERE { ?a ACTIVITY {} } LIMIT 10',
    })
    expect(((await activities.json()) as any).result[0].result).toEqual(['watch_fire'])
  })

  it('moves a Skill on its outcome stream alone, never on assertion', async () => {
    // Profile §14 rule 1: "the Brain proposes, compiles, and narrates; it
    // never promotes." The model is given no say here, and is not asked.
    // Four cycles below, each of which still ends in one completion.
    const runtime = testEnv(new FakeAi(Array.from({ length: 4 }, () => ({ types: [], predicates: [], commands: [], summary: 'Nothing to do.' }))))
    const space = uniqueSpace('skill-lifecycle')

    const created = await post(runtime, space, 'execute_kip', {
      command: `MUTATE {
  UPSERT CONCEPT ?s {
    MATCH { type: "Skill", key: "redeploy" }
    SET FIELDS { name: "Redeploy after a schema change" }
    SET ATTRIBUTES {
      skill_class: "recovery", task_family: "deploy",
      summary: "check the migration target first",
      procedure: "1. verify the target 2. redeploy", status: "proposed"
    }
  }
}`,
    })
    expect(created.status, await text(created)).toBe(200)

    const grade = async (status: string, magnitude: number) => {
      const written = await post(runtime, space, 'execute_kip', {
        command: `MUTATE {
  CREATE EVIDENCE ?e {
    SET FIELDS {
      evidence_class: "outcome", payload: {instrument: "ci"},
      observed_at: "2026-08-31T00:00:00Z"
    }
    SET FACET "OutcomeRecord" {
      task_family: "deploy", outcome_status: :status, magnitude: :magnitude
    }
  }
}`,
        parameters: { status, magnitude },
      })
      expect(written.status, await text(written)).toBe(200)
    }

    const cycle = async () => {
      const response = await post(runtime, space, 'maintenance', { scope: 'quick' })
      expect(response.status, await text(response)).toBe(200)
      return ((await response.json()) as any).result.settlement.skills
    }
    const standing = async () => {
      const response = await post(runtime, space, 'execute_kip_readonly', {
        command:
          'FIND(?s.attributes.status, ?s.facets["SkillUtility"].utility, ?s.facets["SkillUtility"].graded_count) ' +
          'WHERE { ?s CONCEPT {type: "Skill", key: "redeploy"} } LIMIT 1',
      })
      return ((await response.json()) as any).result[0].result[0] as [string, number, number]
    }

    // A poor first showing opens the trial and records the basis (1 of 3).
    await grade('success', 0.5)
    await grade('failure', 0.2)
    await grade('failure', 0.2)
    expect(await cycle()).toMatchObject({ transitions: 1 })
    expect((await standing())[0]).toBe('trialed')

    // The stream then beats that basis over enough runs. Adoption is
    // comparative — better than things were going, not merely good.
    for (let i = 0; i < 6; i += 1) await grade('success', 0.5)
    expect(await cycle()).toMatchObject({ transitions: 1 })
    const [status, utility, graded] = await standing()
    expect(status).toBe('adopted')
    expect(graded).toBe(9)
    expect(utility).toBeCloseTo(7 / 9, 9)

    // Idempotent: the cursor advanced, so a replay grades nothing and cannot
    // promote on arithmetic instead of evidence.
    expect(await cycle()).toMatchObject({ graded: 0, transitions: 0 })

    // One severe matching-condition failure revokes without waiting — the
    // Profile's one sanctioned asymmetry, and it favours demotion.
    await grade('failure', 0.95)
    expect(await cycle()).toMatchObject({ transitions: 1 })
    expect((await standing())[0]).toBe('revoked')

    // Every move left a recomputable verdict: inputs are the graded Evidence,
    // outputs the Skill it moved, and the digest pins the rule and basis.
    const verdicts = await post(runtime, space, 'execute_kip_readonly', {
      command:
        'FIND(?a.parameters_digest, ?a.inputs, ?a.outputs) ' +
        'WHERE { ?a ACTIVITY {activity_class: "lifecycle_verdict"} } LIMIT 10',
    })
    const rows = ((await verdicts.json()) as any).result[0].result as [string, any[], any[]][]
    expect(rows).toHaveLength(3)
    const cited: string[] = []
    for (const [digest, inputs, outputs] of rows) {
      // The rule identity is shared with `anda_brain` on purpose: an auditor
      // must get the same answer whichever deployment wrote the verdict.
      expect(digest).toContain('anda-brain/skill-verdict@1')
      expect(digest).toContain('window=(')
      expect(inputs.every((r: any) => String(r.id).startsWith('E-'))).toBe(true)
      expect(outputs).toHaveLength(1)
      expect(String(outputs[0].id)).toMatch(/^C-\d+$/)
      cited.push(...inputs.map((r: any) => String(r.id)))
    }
    // Each graded outcome is cited by exactly one verdict.
    expect(new Set(cited).size).toBe(cited.length)
    expect(cited).toHaveLength(10)
  })

  it('hands the cycle the signals it cannot go and fetch', async () => {
    // The model gets one completion, so a signal absent from its input is a
    // duty it will not perform. This is why the runtime reads them for it.
    const runtime = testEnv(
      new FakeAi([{ types: [], predicates: [], commands: [], summary: 'Nothing to do.' }]),
    )
    const space = uniqueSpace('assessment')
    await post(runtime, space, 'execute_kip', { command: FORMATION_PLAN })
    const past = new Date(Date.now() - 86_400_000).toISOString()
    await post(runtime, space, 'execute_kip', {
      command: `MUTATE {
  UPSERT CONCEPT ?w {
    MATCH { type: "Watch", key: "due" }
    SET FIELDS { name: "due" }
    SET ATTRIBUTES {
      watch_class: "silence", summary: "s", condition: "c",
      status: "armed", due_at: :due
    }
  }
}`,
      parameters: { due: past },
    })

    const response = await post(runtime, space, 'maintenance', { scope: 'full' })
    expect(response.status, await text(response)).toBe(200)

    // The prompt the model actually saw.
    const prompt = JSON.parse(lastUserPayload(runtime))
    expect(prompt.snapshot.assessment.space_seq).toBeGreaterThan(0)
    // Fired by the settlement in this same cycle, and now waiting for the
    // action gate — which is cognition, not arithmetic.
    expect(prompt.snapshot.assessment.fired_watches).toHaveLength(1)
    expect(prompt.snapshot.assessment.armed_watches).toHaveLength(0)
    expect(prompt.snapshot.assessment.predicates.prefers).toBe(1)
  })

  it('lets maintenance set retention, which is what §20 review is for', async () => {
    // `SET RETENTION` used to be refused at the gate because the engine had not
    // built it. It has, so §20 Retention Review and §25 Retention Expiry have a
    // mechanism here now: a retention class and an `expires_at` are storage
    // policy, and the removal they schedule is a host-run sweep.
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [
            'MUTATE { CREATE CONCEPT ?c { TYPE "Event" NAME "Kept" SET ATTRIBUTES {summary: "an event worth keeping"} } }',
            'SET RETENTION ?e { retention_class: "standard", expires_at: "2030-01-01T00:00:00Z" } WHERE { ?e CONCEPT {} } LIMIT 5',
          ],
          summary: 'Scheduled some events to expire.',
        },
      ]),
    )
    const space = uniqueSpace('maintenance-retention')
    const response = await post(runtime, space, 'maintenance', { scope: 'full' })
    expect(response.status).toBe(200)

    // And it landed: the retention block is on the element, not merely accepted.
    const found = await post(runtime, space, 'execute_kip', {
      operations: [
        { command: 'FIND(?c.retention.retention_class) WHERE { ?c CONCEPT {type: "Event"} }' },
      ],
    })
    expect(await found.json()).toMatchObject({
      result: [{ result: ['standard'] }],
    })
  })

  it('refuses a legal hold in a maintenance plan, in either direction', async () => {
    // §60.3: a hold blocks erasure for everyone, whatever the reference
    // policy says, and the authority to set or lift one SHOULD be scoped apart
    // from ordinary retention management. Content that could place a hold
    // could make itself undeletable, and content that could clear one could
    // unblock an erasure somebody placed a hold to stop. Neither is a decision
    // to reach from a graph snapshot.
    //
    // Refused at the gate, so the whole plan fails before anything runs — a
    // batch is not a transaction, and half-committing before failing would
    // leave the caller unable to tell what landed.
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [
            'MUTATE { CREATE CONCEPT ?c { TYPE "Event" NAME "Kept" SET ATTRIBUTES {summary: "an event worth keeping"} } }',
            'SET RETENTION ?e { legal_hold: true } WHERE { ?e CONCEPT {} } LIMIT 5',
          ],
          summary: 'Hold some events.',
        },
      ]),
    )
    const space = uniqueSpace('maintenance-legal-hold')
    const response = await post(runtime, space, 'maintenance', { scope: 'full' })
    expect(response.status).toBe(422)
    expect(await response.text()).toContain('cannot place or lift a legal hold')

    // Nothing from the plan committed, including the legal first command.
    const info = await get(runtime, space, 'info')
    expect(((await info.json()) as { result: { concepts: number } }).result.concepts).toBe(1)
  })

  it('leaves a sweeping merge to the engine, which refuses it better', async () => {
    // `MERGE CONCEPT` is the one selecting clause KIP gives no LIMIT, and the
    // gate used to read that as a hole and refuse every guarded merge. It is
    // not one: the WHERE is a guard, and the engine resolves each operand
    // separately and refuses one that binds more than one Concept — naming the
    // counts and asking for a stable identity, which is a better answer than
    // the gate could give and does not cost the legitimate one-pair case.
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [
            'MUTATE { CREATE CONCEPT ?a { TYPE "Event" NAME "First" SET ATTRIBUTES {summary: "one event"} } }',
            'MUTATE { CREATE CONCEPT ?b { TYPE "Event" NAME "Second" SET ATTRIBUTES {summary: "another event"} } }',
            'MERGE CONCEPT ?dup INTO ?keep WHERE { ?dup CONCEPT {type: "Event"} ?keep CONCEPT {type: "Event"} }',
          ],
          summary: 'Merging duplicates.',
        },
      ]),
    )
    const space = uniqueSpace('maintenance-merge')
    const response = await post(runtime, space, 'maintenance', { scope: 'full' })

    // The gate passed it, so the two creates ran; the merge is what failed,
    // and it failed with the engine's own diagnosis — a registry code a caller
    // can switch on, the counts that made it ambiguous, and what to write
    // instead. None of that is available to a gate reading the parse tree.
    expect(response.status).toBe(422)
    const failure = await response.text()
    expect(failure).toContain('IdentitySelectorRequired')
    expect(failure).toContain('needs exactly one source and one target')

    const info = await get(runtime, space, 'info')
    expect(((await info.json()) as { result: { concepts: number } }).result.concepts).toBe(3)
  })

  it('accepts the maintenance parameter names the Rust service documents', async () => {
    const runtime = testEnv(
      new FakeAi([{ types: [], predicates: [], commands: [], summary: 'Nothing to do.' }]),
    )
    const response = await post(
      runtime,
      uniqueSpace('maintenance-params'),
      'maintenance',
      {
        scope: 'quick',
        parameters: {
          memory_strength_decay_factor: 0.95,
          stale_event_threshold_days: 30,
          unconsolidated_max_backlog: 20,
          orphan_max_count: 20,
        },
      },
    )
    expect(response.status).toBe(200)

    // ... and still refuses a decay factor that would erase accessibility in
    // one sweep, rather than clamping it silently.
    const bad = testEnv(new FakeAi([]))
    const rejected = await post(bad, uniqueSpace('maintenance-params'), 'maintenance', {
      parameters: { memory_strength_decay_factor: 0 },
    })
    expect(rejected.status).toBe(400)
    expect(await rejected.text()).toContain('memory_strength_decay_factor')
  })

  it('designates the $self Concept its Recall policy claims to speak for', async () => {
    // The Person was always bootstrapped; the designation was not, so
    // `DESCRIBE PRIMER` reported no cognitive identity next to a prompt that
    // opens "you operate on behalf of $self".
    const runtime = testEnv(new FakeAi([]))
    const space = uniqueSpace('self-identity')
    const response = await post(runtime, space, 'execute_kip_readonly', {
      command: 'DESCRIBE PRIMER',
    })
    expect(response.status).toBe(200)
    const body = (await response.json()) as {
      result: { result: { cognitive_identity?: unknown } }[]
    }
    const identity = JSON.stringify(body.result[0]?.result?.cognitive_identity ?? null)
    expect(identity).toContain('C-')
  })

  it('metabolizes memory strength and reports the change', async () => {
    const space = uniqueSpace('maintenance-update')
    const runtime = testEnv(
      new FakeAi([
        {
          types: [],
          predicates: [],
          commands: [
            `UPDATE ?c
             SET FACET "MnemonicState" { memory_strength: 0.5 }
             WHERE { ?c CONCEPT {type: "Preference", name: "Alice style"} }
             LIMIT 1`,
          ],
          summary: 'Metabolized one preference.',
        },
      ]),
    )
    await post(runtime, space, 'execute_kip', {
      command: `CREATE CONCEPT ?c {
        TYPE "Preference"
        NAME "Alice style"
        SET FACET "MnemonicState" { memory_strength: 0.9 }
      }`,
    })

    const response = await post(runtime, space, 'maintenance', {
      trigger: 'on_demand',
      scope: 'quick',
    })
    expect(await text(response)).toBe('')
    const body = await response.json<Record<string, any>>()
    expect(body.result.changed).toMatchObject({ total: 1, updated: 1 })
  })

  it('rejects non-string maintenance enums and non-ISO timestamps', async () => {
    const runtime = testEnv(new FakeAi([]))
    const invalidScope = await post(runtime, uniqueSpace('scope'), 'maintenance', {
      scope: ['quick'],
    })
    expect(invalidScope.status).toBe(400)

    const invalidTimestamp = await post(runtime, uniqueSpace('timestamp'), 'formation', {
      messages: [{ role: 'user', content: 'Remember this.' }],
      timestamp: '2026',
    })
    expect(invalidTimestamp.status).toBe(400)

    const invalidRole = await post(runtime, uniqueSpace('role'), 'formation', {
      messages: [{ role: ['user'], content: 'Remember this.' }],
    })
    expect(invalidRole.status).toBe(400)
  })

  it('returns route and internal errors without parsing or leaking details', async () => {
    const runtime = testEnv(new FakeAi([new Error('provider-secret-detail')]))
    const missing = await handleRequest(
      new Request(`https://brain.example/v1/${uniqueSpace('missing')}/unknown`, {
        method: 'POST',
      }),
      runtime,
    )
    expect(missing.status).toBe(404)

    const failed = await post(runtime, uniqueSpace('internal'), 'formation', {
      messages: [{ role: 'user', content: 'Remember this.' }],
    })
    expect(failed.status).toBe(500)
    expect(await failed.text()).toBe('{"error":{"message":"internal error"}}')
  })

  it('reports what this Space holds', async () => {
    const space = uniqueSpace('info')
    const runtime = testEnv(new FakeAi([]))
    await post(runtime, space, 'execute_kip', {
      command: 'CREATE CONCEPT ?c { TYPE "Person" NAME "Alice" }',
    })
    const info = await (await get(runtime, space, 'info')).json<Record<string, any>>()
    expect(info.result).toMatchObject({ space_id: space, kip: '2.0', concepts: 2 })
    // One activation on construction; a restart is not a schema change.
    expect(info.result.schema_environment_version).toBe(1)
  })

  it('carries the reference policy and the syntax card into the prompt', () => {
    const [system, user] = formationMessages(
      { types: [] },
      { messages: [{ role: 'user', content: 'remember-newest-turn' }] },
      '2026-08-20T00:00:00Z',
    )
    expect(system?.content).toContain('Reference Anda Brain Formation Policy')
    expect(system?.content).toContain('Anda Brain Worker deployment contract')
    expect(system?.content).toContain('KIP 2.0')
    expect(user?.content).toContain('remember-newest-turn')

    const maintenance = maintenanceMessages(
      { trigger: 'on_demand', scope: 'quick' },
      [],
      '2026-08-20T00:00:00Z',
    )[0]?.content
    expect(maintenance).toContain('never decay Assertion confidence over time')
    // KIP 1.x taught the language from a hand-written summary in this file.
    expect(maintenance).not.toContain('UPSERT {')
  })

  it('keeps long prompt payloads valid and preserves their newest data', () => {
    const rendered = boundedJson(
      { oldest: 'a'.repeat(1_000), newest: 'remember-latest-message' },
      240,
    )
    expect(rendered.length).toBeLessThanOrEqual(240)
    const value = JSON.parse(rendered) as Record<string, any>
    expect(value.truncated).toBe(true)
    expect(value.tail).toContain('remember-latest-message')
  })

  it('protects all space endpoints when BRAIN_API_KEY is configured', async () => {
    const runtime = { ...testEnv(new FakeAi([])), BRAIN_API_KEY: 'secret-key' }
    const request = new Request(`https://brain.example/v1/${uniqueSpace('auth')}/info`)
    const response = await handleRequest(request, runtime)
    expect(response.status).toBe(401)
  })
})

function testEnv(ai: AiBinding): Env {
  return {
    BRAIN: env.BRAIN as unknown as Env['BRAIN'],
    AI: ai,
    AI_MODEL: 'test-model',
  }
}

function post(runtime: Env, space: string, action: string, body: unknown): Promise<Response> {
  return handleRequest(
    new Request(`https://brain.example/v1/${space}/${action}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    }),
    runtime,
  )
}

function get(runtime: Env, space: string, action: string): Promise<Response> {
  return handleRequest(new Request(`https://brain.example/v1/${space}/${action}`), runtime)
}

/** The failure body when a response was not 200, so a test says why. */
async function text(response: Response): Promise<string> {
  return response.status === 200 ? '' : await response.clone().text()
}

/** The transaction outcome §81 moved into the operation's `extensions` slot. */
function outcomeOf(body: Record<string, any>, index = 0): Record<string, any> {
  return body.result[index].extensions['kip-do/outcome']
}

function uniqueSpace(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`
}

function failingReadBrain(): BrainRpc {
  // `status` is what a caller decides on (§82): it is required on the wire and
  // not derivable from the other fields, so a stub that carried only an error
  // would exercise a shape the engine never produces.
  const failure = {
    status: 'failed',
    error: {
      code: 'InternalError',
      category: 'system',
      message: 'storage unavailable',
      hint: 'retry later',
      retry: { class: 'safe_same_request' },
    },
  }
  const unsupported = async (): Promise<never> => {
    throw new Error('unexpected brain call')
  }
  return {
    declareSymbols: unsupported,
    describePrimer: async () => ({ status: 'succeeded', result: {} }),
    executeFormationPlan: unsupported,
    executeKip: unsupported,
    executeKipBatch: unsupported,
    executeKipReadonlyBatch: async () => [failure],
    executeMaintenancePlan: unsupported,
    stats: unsupported,
    vocabulary: unsupported,
  } as unknown as BrainRpc
}
