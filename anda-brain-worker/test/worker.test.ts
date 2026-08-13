import { env } from 'cloudflare:test'
import { describe, expect, it } from 'vitest'
import { handleRequest } from '../src/index.js'
import { boundedJson, formationMessages, maintenanceMessages } from '../src/prompts.js'
import type { AiBinding, BrainRpc, Env } from '../src/types.js'

class FakeAi implements AiBinding {
  constructor(private readonly responses: unknown[]) {}

  async run(): Promise<unknown> {
    const response = this.responses.shift()
    if (response === undefined) throw new Error('unexpected AI call')
    if (response instanceof Error) throw response
    return {
      response,
      usage: { input_tokens: 10, output_tokens: 5 },
    }
  }
}

describe('Anda Brain Worker', () => {
  it('forms durable memory and retrieves it with read-only KIP', async () => {
    const space = uniqueSpace('formation')
    const ai = new FakeAi([
      {
        commands: [
          `UPSERT {
            CONCEPT ?preference {
              {type: "Preference", name: "alice:concise_answers"}
              SET ATTRIBUTES {
                preference_class: "communication",
                description: "Alice prefers concise answers"
              }
            }
          }
          WITH METADATA {
            source: "test-conversation",
            created_at: "2026-08-12T00:00:00Z",
            confidence: 0.98,
            status: "active"
          }`,
        ],
        summary: 'Stored Alice’s response-style preference.',
      },
    ])
    const runtime = testEnv(ai)

    const formed = await post(runtime, space, 'formation', {
      messages: [{ role: 'user', content: 'Please keep your answers concise.' }],
      context: { counterparty: 'alice' },
      timestamp: '2026-08-12T00:00:00Z',
    })
    expect(formed.status).toBe(200)
    const formedBody = await formed.json<Record<string, any>>()
    expect(formedBody.result.stored.concepts).toBe(1)

    const recalled = await post(runtime, space, 'execute_kip_readonly', {
      command:
        'FIND(?preference) WHERE { ?preference {type: "Preference", name: "alice:concise_answers"} } LIMIT 5',
    })
    expect(recalled.status).toBe(200)
    expect(JSON.stringify(await recalled.json())).toContain('concise answers')
  })

  it('uses bounded read-only retrieval before synthesizing a recall answer', async () => {
    const space = uniqueSpace('recall')
    const runtime = testEnv(
      new FakeAi([
        {
          commands: [
            'FIND(?preference) WHERE { ?preference {type: "Preference"} } LIMIT 5',
          ],
        },
        {
          answer: 'Alice prefers concise answers.',
          found: true,
          uncertainty: 0.05,
        },
      ]),
    )

    await post(runtime, space, 'execute_kip', {
      command: `UPSERT {
        CONCEPT ?preference {
          {type: "Preference", name: "alice:concise_answers"}
          SET ATTRIBUTES { description: "Alice prefers concise answers" }
        }
      }`,
    })

    const response = await post(runtime, space, 'recall_structured', {
      query: 'How should I answer Alice?',
      context: { counterparty: 'alice' },
    })
    expect(response.status).toBe(200)
    const body = await response.json<Record<string, any>>()
    expect(body.result.answer).toBe('Alice prefers concise answers.')
    expect(body.result.found).toBe(true)
    expect(body.result.memories[0].entity).toMatch(/^C:/)
  })

  it('discards unbounded model retrieval plans and keeps the bounded fallback', async () => {
    const runtime = testEnv(
      new FakeAi([
        {
          commands: [
            'FIND(?person) WHERE { ?person {type: "Person"} }',
            'SEARCH CONCEPT "person" LIMIT 500',
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
  })

  it('surfaces KIP retrieval failures instead of reporting a false miss', async () => {
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

  it('rejects mutations through the read-only endpoint', async () => {
    const runtime = testEnv(new FakeAi([]))
    const response = await post(runtime, uniqueSpace('readonly'), 'execute_kip_readonly', {
      command:
        'UPSERT { CONCEPT ?person { {type: "Person", name: "mallory"} } }',
    })
    expect(response.status).toBe(400)
    expect(await response.text()).toContain('read-only KIP')
  })

  it('rejects a model plan that tries to use formation as a general KIP tool', async () => {
    const runtime = testEnv(
      new FakeAi([{ commands: ['DESCRIBE PRIMER'], summary: 'unsafe plan' }]),
    )
    const response = await post(runtime, uniqueSpace('formation-guard'), 'formation', {
      messages: [{ role: 'user', content: 'Remember that I like tea.' }],
    })
    expect(response.status).toBe(422)
    expect(await response.text()).toContain('formation accepts only KIP UPSERT')
  })

  it('runs a no-op maintenance cycle against a real graph snapshot', async () => {
    const runtime = testEnv(
      new FakeAi([{ commands: [], summary: 'The graph needs no maintenance.' }]),
    )
    const response = await post(runtime, uniqueSpace('maintenance'), 'maintenance', {
      trigger: 'on_demand',
      scope: 'quick',
    })
    expect(response.status).toBe(200)
    const body = await response.json<Record<string, any>>()
    expect(body.result.scope).toBe('quick')
    expect(body.result.commands).toBe(0)
  })

  it('rejects an unbounded maintenance UPDATE plan', async () => {
    const runtime = testEnv(
      new FakeAi([
        {
          commands: [
            'UPDATE ?event SET ATTRIBUTES { status: "archived" } WHERE { ?event {type: "Event"} }',
          ],
          summary: 'Unsafe bulk update.',
        },
      ]),
    )
    const response = await post(
      runtime,
      uniqueSpace('maintenance-limit'),
      'maintenance',
      { scope: 'full' },
    )
    expect(response.status).toBe(422)
    expect(await response.text()).toContain('LIMIT 20')
  })

  it('reports UPDATE work performed by maintenance', async () => {
    const space = uniqueSpace('maintenance-update')
    const runtime = testEnv(
      new FakeAi([
        {
          commands: [
            `UPDATE ?preference
             SET ATTRIBUTES { description: "Updated by maintenance" }
             WHERE { ?preference {type: "Preference", name: "alice:style"} }
             LIMIT 1`,
          ],
          summary: 'Updated one preference.',
        },
      ]),
    )
    await post(runtime, space, 'execute_kip', {
      command: `UPSERT {
        CONCEPT ?preference {
          {type: "Preference", name: "alice:style"}
          SET ATTRIBUTES { description: "Original" }
        }
      }`,
    })

    const response = await post(runtime, space, 'maintenance', {
      trigger: 'on_demand',
      scope: 'quick',
    })
    expect(response.status).toBe(200)
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

  it('keeps long prompt payloads valid and preserves their newest data', () => {
    const rendered = boundedJson(
      { oldest: 'a'.repeat(1_000), newest: 'remember-latest-message' },
      240,
    )
    expect(rendered.length).toBeLessThanOrEqual(240)
    const value = JSON.parse(rendered) as Record<string, any>
    expect(value.truncated).toBe(true)
    expect(value.tail).toContain('remember-latest-message')

    const formationPayload = formationMessages(
      { schema: 'p'.repeat(20_000) },
      { messages: [{ role: 'user', content: 'remember-newest-turn' }] },
      '2026-08-12T00:00:00Z',
    )[1]?.content
    expect(JSON.parse(formationPayload ?? '{}').tail).toContain('remember-newest-turn')

    const maintenancePrompt = maintenanceMessages(
      { trigger: 'on_demand', scope: 'quick' },
      [],
      '2026-08-12T00:00:00Z',
    )[0]?.content
    expect(maintenancePrompt).toContain('UPDATE ?target')
    expect(maintenancePrompt).not.toContain('Write only with a complete UPSERT')
  })

  it('protects all space endpoints when BRAIN_API_KEY is configured', async () => {
    const runtime = { ...testEnv(new FakeAi([])), BRAIN_API_KEY: 'secret-key' }
    const request = new Request(
      `https://brain.example/v1/${uniqueSpace('auth')}/info`,
    )
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

function uniqueSpace(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`
}

function failingReadBrain(): BrainRpc {
  const failure = {
    error: {
      code: 'KIP_5001',
      name: 'InternalError',
      message: 'tokenizer unavailable',
      hint: 'retry later',
    },
  }
  const unsupported = async (): Promise<never> => {
    throw new Error('unexpected brain call')
  }
  return {
    describePrimer: async () => ({ result: {} }),
    executeFormationPlan: unsupported,
    executeKip: unsupported,
    executeKipBatch: unsupported,
    executeKipReadonlyBatch: async () => [failure],
    executeMaintenancePlan: unsupported,
    stats: unsupported,
  } as BrainRpc
}
