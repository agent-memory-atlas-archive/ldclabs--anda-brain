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
      observed_at: "2026-08-20T00:00:00.000Z"
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
  it('looks up embedded references before executing the final formation plan', async () => {
    const space = uniqueSpace('formation_reference')
    const ai = new FakeAi([
      { types: [], predicates: [], commands: [], summary: '', references: [{ document: 'syntax', section: 'kml', offset: 0 }] },
      { types: [], predicates: [], commands: [FORMATION_PLAN], summary: 'Stored after checking syntax.' },
    ])
    const response = await post(testEnv(ai), space, 'formation', {
      messages: [{ role: 'user', content: 'Please keep your answers concise.' }],
    })
    expect(response.status).toBe(200)
    const body = await response.json<Record<string, any>>()
    expect(body.result.commands).toBe(1)
    expect(body.result.usage).toEqual({ input_tokens: 20, output_tokens: 10 })
    expect(ai.calls).toHaveLength(2)
    const receipt = JSON.parse(ai.calls[1]![0]!.content.split('\n\n# Embedded reference lookup result\n').at(-1)!)
    expect(receipt.kind).toBe('embedded_protocol_references')
    expect(receipt.results[0].reference.document).toBe('syntax')
  })

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
      timestamp: '2026-08-20T00:00:00.000Z',
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

  it('mints the observation itself, so the model never retypes it', async () => {
    // §71.1, and the point is fidelity: the payload stored is the payload the
    // runtime received. The plan below never contains the sentence — it cites
    // `:msg1` — so finding the sentence verbatim in the Evidence proves it did
    // not pass through model-generated text on the way in (§88.12).
    const said = 'Please keep answers concise — I mean it, ≤ 3 sentences.'
    const plan = {
      types: [],
      predicates: [],
      commands: [
        `MUTATE {
          UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS { name: "Alice" } }
          CREATE CONCEPT ?concise {
            TYPE "Preference"
            NAME "Alice concise answers"
            SET ATTRIBUTES { preference_class: "communication" }
          }
          ASSERT ?a (?alice, "prefers", ?concise) {
            by: ?alice, mode: "stated", confidence: 0.95, evidence: :msg1
          }
        }`,
      ],
      summary: 'Stored Alice’s response-style preference.',
    }
    const space = uniqueSpace('formation-ingest')
    const runtime = testEnv(new FakeAi([plan, plan]))
    const send = () =>
      post(runtime, space, 'formation', {
        messages: [{ role: 'user', content: said }],
        context: { counterparty: 'alice', source: 'chat_thread_123' },
        timestamp: '2026-08-20T00:00:00.000Z',
      })

    expect(await text(await send())).toBe('')

    const read = async () =>
      (
        (await (
          await post(runtime, space, 'execute_kip_readonly', {
            command:
              'FIND(?e.payload, ?e.evidence_class, ?e.observed_at) WHERE { ?e EVIDENCE {} } LIMIT 5',
          })
        ).json()) as { result: { result: unknown[] }[] }
      ).result[0]?.result

    expect(await read()).toEqual([
      [
        // The whole message, role included: who said a thing is part of what
        // was observed, and a bare string would lose it. Verbatim, em dash and
        // `≤` intact — the plan above never contains this sentence.
        { mode: 'inline', inline: { role: 'user', content: said } },
        // From the speaker's role, not from anything the model chose.
        'user_statement',
        '2026-08-20T00:00:00.000Z',
      ],
    ])

    // And the Assertion cites it, which is what makes the record evidence for
    // something rather than an observation nobody acted on.
    const cited = await post(runtime, space, 'execute_kip_readonly', {
      command: 'FIND(?a.evidence) WHERE { ?a ASSERTION {} } LIMIT 5',
    })
    expect(JSON.stringify(await cited.json())).toContain('E-1')

    // Sent again, `context.source` gives the mint a stable `client_key`, so the
    // resend resolves to the record the first attempt wrote (§52.1). Without
    // that, a caller whose response was lost would double every observation it
    // ever made.
    expect(await text(await send())).toBe('')
    expect(await read()).toHaveLength(1)
  })

  it('publishes a symbol the Profile does not have, then writes with it', async () => {
    const space = uniqueSpace('vocabulary')
    const runtime = testEnv(
      new FakeAi([
        {
          // `Assertion` is a Core element kind, which §20.13 forbids a package
          // from shadowing — refused here rather than at package installation,
          // where it would take the whole publish down.
          types: ['Project', 'Assertion'],
          // `Treats` is malformed as a predicate; it comes back refused rather
          // than published under a tidied-up name.
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
      rejected: ['Assertion', 'Treats'],
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
          commands: [plan, `TRANSITION :old TO "superseded" BY :new`],
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
      command: 'TRANSITION :old TO "superseded" BY :new',
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
      command:
        'TRANSITION ?c TO "archived" WHERE { ?c CONCEPT {type: "Person", key: "nobody"} } LIMIT 1',
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

  it('rejects a formation TRANSITION that removes memory, or hides its state', async () => {
    // Six statements collapsed into one, so the split between cognition and
    // custody now falls inside `TRANSITION`. The gate reads the state — and
    // refuses one it cannot resolve to a literal, because the engine would
    // resolve it later and "later" is past the gate.
    for (const [command, expected] of [
      ['TRANSITION :a TO "tombstoned"', 'formation cannot TRANSITION memory to'],
      ['TRANSITION :a TO :state', 'as a literal'],
    ] as const) {
      const runtime = testEnv(
        new FakeAi([{ types: [], predicates: [], commands: [command], summary: 'unsafe' }]),
      )
      const response = await post(
        runtime,
        uniqueSpace('formation-transition'),
        'formation',
        { messages: [{ role: 'user', content: 'Forget that.' }] },
      )
      expect(response.status, command).toBe(422)
      expect(await response.text()).toContain(expected)
    }
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
      'TRANSITION ?event TO "archived" WHERE { ?event CONCEPT {type: "Event"} }',
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
            'SET RETENTION ?e { retention_class: "standard", expires_at: "2030-01-01T00:00:00.000Z" } WHERE { ?e CONCEPT {} } LIMIT 5',
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
    expect(failure).toContain('IdentityMergeConflict')
    expect(failure).toContain('requires exactly one source and one target')

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

  it('rejects non-string maintenance enums and invalid message roles', async () => {
    const runtime = testEnv(new FakeAi([]))
    const invalidScope = await post(runtime, uniqueSpace('scope'), 'maintenance', {
      scope: ['quick'],
    })
    expect(invalidScope.status).toBe(400)

    const invalidRole = await post(runtime, uniqueSpace('role'), 'formation', {
      messages: [{ role: ['user'], content: 'Remember this.' }],
    })
    expect(invalidRole.status).toBe(400)
  })

  it('normalizes external timestamps and captures malformed ones at receipt time', async () => {
    for (const [timestamp, expected] of [
      ['2026-08-20T00:00:00Z', '2026-08-20T00:00:00.000Z'],
      [' 2026-08-20T08:00:00.123456+08:00 ', '2026-08-20T00:00:00.123Z'],
      ['2026', undefined],
      ['not a timestamp', undefined],
      ['2026-02-30T00:00:00Z', undefined],
    ] as const) {
      const runtime = testEnv(new FakeAi([{
        types: [], predicates: [], summary: 'captured',
        commands: ['CREATE ACTIVITY ?a { SET FIELDS {activity_class: "extraction", status: "completed"} SET STRUCTURAL {("inputs", :msg1)} }'],
      }]))
      const space = uniqueSpace('timestamp-compatible')
      const before = Date.now()
      const written = await post(runtime, space, 'formation', {
        messages: [{role: 'user', content: 'Original message.'}], timestamp,
      })
      expect(written.status, await written.text()).toBe(200)
      const read = await post(runtime, space, 'execute_kip_readonly', {
        command: 'FIND(?e.observed_at, ?e.payload) WHERE {?e EVIDENCE {}} LIMIT 10',
      })
      const body = await read.json<Record<string, any>>()
      const rows = body.result[0].result
      expect(rows).toHaveLength(1)
      expect(JSON.stringify(rows[0][1])).toContain('Original message.')
      if (expected) expect(rows[0][0]).toBe(expected)
      else {
        expect(Date.parse(rows[0][0])).toBeGreaterThanOrEqual(before)
        expect(Date.parse(rows[0][0])).toBeLessThanOrEqual(Date.now())
      }
    }
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
      '2026-08-20T00:00:00.000Z',
    )
    expect(system?.content).toContain('Reference Anda Brain Formation Policy')
    expect(system?.content).toContain('Anda Brain Worker deployment contract')
    expect(system?.content).toContain('### 5. Runtime Envelope')
    expect(user?.content).toContain('remember-newest-turn')

    const maintenance = maintenanceMessages(
      { trigger: 'on_demand', scope: 'quick' },
      [],
      '2026-08-20T00:00:00.000Z',
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
    beginProcessing: async () => 0,
    checkProcessing: async () => {},
    executeAgentRead: async () => [failure],
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
