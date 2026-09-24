import { AiResponseError } from './ai.js'
import { processingErrorCode } from './processing.js'
import { validateForget } from './forget.js'
import { assertReadonlyOperations } from './kip.js'
import { AndaBrain } from './brain.js'
import {
  formMemory,
  maintainMemory,
  OperationError,
  probeMemory,
  recallMemory,
} from './operations.js'
import type { BrainRpc, Env } from './types.js'
import {
  parseFormationInput,
  parseKipInput,
  parseMaintenanceInput,
  parsePromoteInput,
  parseRecallInput,
  ValidationError,
} from './validation.js'

export { AndaBrain }

const MAX_BODY_BYTES = 256 * 1024
const SPACE_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/
const GET_ONLY = new Set(['info', 'formation_status', 'vocabulary', 'memory/attention', 'schema/drafts'])
const POST_ACTIONS = new Set([
  'formation',
  'memory/forget',
  'recall',
  'recall_structured',
  'maintenance',
  'probe',
  'execute_kip_readonly',
  'execute_kip',
  'schema/promote',
])

class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly data?: unknown,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

export async function handleRequest(request: Request, env: Env): Promise<Response> {
  try {
    if (request.method === 'OPTIONS') return corsPreflight()

    const url = new URL(request.url)
    if (url.pathname === '/healthz') return json({ ok: true })
    if (url.pathname === '/') {
      return json({
        name: 'Anda Brain Worker',
        description: 'Compact graph memory for AI agents on Cloudflare Workers.',
        engine: '@ldclabs/kip-do',
        kip: '2.0',
      })
    }

    const match = /^\/v1\/([^/]+)\/([^/]+(?:\/[^/]+)?)$/.exec(url.pathname)
    if (!match) throw new ApiError('not found', 404)
    authorize(request, env)

    let spaceId: string
    try {
      spaceId = decodeURIComponent(match[1] ?? '')
    } catch {
      throw new ApiError('invalid space id', 400)
    }
    const action = match[2] ?? ''
    if (!SPACE_ID.test(spaceId)) throw new ApiError('invalid space id', 400)
    if (!GET_ONLY.has(action) && !POST_ACTIONS.has(action)) {
      throw new ApiError('not found', 404)
    }
    const brain = env.BRAIN.getByName(spaceId) as unknown as BrainRpc

    if (GET_ONLY.has(action)) {
      if (request.method !== 'GET') throw new ApiError('method not allowed', 405)
      if (action === 'info') {
        return ok({ space_id: spaceId, ...(await brain.stats()) })
      }
      if (action === 'vocabulary') {
        return ok(await brain.vocabulary())
      }
      if (action === 'schema/drafts') {
        return ok(await brain.schemaDrafts())
      }
      if (action === 'memory/attention') {
        const cursor = url.searchParams.get('attention_cursor') ?? undefined
        const limitText = url.searchParams.get('limit')
        const limit = limitText === null ? undefined : Number(limitText)
        try {
          return ok(await brain.recallAttention({
            ...(cursor === undefined ? {} : { attention_cursor: cursor }),
            ...(limit === undefined ? {} : { limit }),
          }))
        } catch (error) {
          if (error instanceof Error && /attention (cursor|limit)/.test(error.message)) throw new ValidationError(error.message)
          throw error
        }
      }
      return ok({
        formation_processing: false,
        maintenance_processing: false,
        mode: 'synchronous',
      })
    }
    if (request.method !== 'POST') throw new ApiError('method not allowed', 405)

    const body = await readJson(request)
    switch (action) {
      case 'memory/forget':
        try { validateForget(body) } catch (error) { throw new ValidationError((error as Error).message) }
        return ok(await brain.forgetMemory(body))
      case 'formation':
        return ok(await formMemory(env, brain, parseFormationInput(body)))
      case 'recall':
      case 'recall_structured':
        return ok(await recallMemory(env, brain, parseRecallInput(body)))
      case 'maintenance':
        return ok(await maintainMemory(env, brain, parseMaintenanceInput(body)))
      case 'probe':
        return ok(await probeMemory(brain, parseRecallInput(body)))
      case 'execute_kip_readonly': {
        const batch = parseKipInput(body)
        // Gated here as well as inside the object: a 400 that names the offence
        // is a better answer than a per-operation error, and the object's own
        // gate is what makes this one an early message rather than the only
        // thing standing between a mutation and the graph.
        try {
          assertReadonlyOperations(batch.operations)
        } catch (error) {
          throw new ApiError(
            error instanceof Error ? error.message : 'invalid read-only KIP',
            400,
          )
        }
        return ok(await brain.executeKipReadonlyBatch(batch.operations, batch.execution))
      }
      case 'schema/promote': {
        // The owner's Schema migration (Spec §20.16): this API key already
        // holds the administrative `execute_kip`, which is where
        // `manage_schema` lives on this deployment.
        const input = parsePromoteInput(body)
        try {
          return ok(await brain.promoteDraftSymbol(input))
        } catch (error) {
          throw new ApiError(error instanceof Error ? error.message : 'promotion failed', 422)
        }
      }
      case 'execute_kip': {
        const batch = parseKipInput(body)
        // `context` and `read` are the object's own concerns on this path, so
        // they go unset; `execution` is the caller's and rides the fourth slot
        // the engine's own batch signature puts it in.
        return ok(
          await brain.executeKipBatch(batch.operations, undefined, undefined, batch.execution),
        )
      }
      default:
        throw new ApiError('not found', 404)
    }
  } catch (error) {
    if (error instanceof AiResponseError) return fail(error.code === 'model_timeout' ? 'model request deadline exceeded' : 'model call failed', error.code === 'model_timeout' ? 504 : 502, { code: error.code, usage: error.usage })
    const code = processingErrorCode(error)
    if (code) return fail(code, 409, { code })
    if (error instanceof ApiError) return fail(error.message, error.status, error.data)
    if (error instanceof ValidationError) return fail(error.message, 400)
    if (error instanceof OperationError) return fail(error.message, error.status, error.data)
    console.error('request failed', error)
    return fail('internal error', 500)
  }
}

function authorize(request: Request, env: Env): void {
  const expected = env.BRAIN_API_KEY?.trim()
  if (!expected) return
  const header = request.headers.get('authorization') ?? ''
  const supplied = header.startsWith('Bearer ') ? header.slice(7) : ''
  if (!constantTimeEqual(supplied, expected)) {
    throw new ApiError('unauthorized', 401)
  }
}

function constantTimeEqual(left: string, right: string): boolean {
  const a = new TextEncoder().encode(left)
  const b = new TextEncoder().encode(right)
  const length = Math.max(a.length, b.length)
  let different = a.length ^ b.length
  for (let index = 0; index < length; index += 1) {
    different |= (a[index] ?? 0) ^ (b[index] ?? 0)
  }
  return different === 0
}

async function readJson(request: Request): Promise<unknown> {
  const declared = Number(request.headers.get('content-length') ?? 0)
  if (Number.isFinite(declared) && declared > MAX_BODY_BYTES) {
    throw new ApiError('request body too large', 413)
  }
  const text = await request.text()
  if (new TextEncoder().encode(text).byteLength > MAX_BODY_BYTES) {
    throw new ApiError('request body too large', 413)
  }
  if (!text.trim()) throw new ValidationError('request body cannot be empty')
  try {
    return JSON.parse(text)
  } catch {
    throw new ValidationError('request body must be valid JSON')
  }
}

function ok(result: unknown): Response {
  return json({ result })
}

function fail(message: string, status: number, data?: unknown): Response {
  return json(
    {
      error: {
        message,
        ...(data === undefined ? {} : { data }),
      },
    },
    status,
  )
}

function json(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
      'access-control-allow-origin': '*',
      'cache-control': 'no-store',
    },
  })
}

function corsPreflight(): Response {
  return new Response(null, {
    status: 204,
    headers: {
      'access-control-allow-origin': '*',
      'access-control-allow-methods': 'GET, POST, OPTIONS',
      'access-control-allow-headers': 'authorization, content-type',
      'access-control-max-age': '86400',
    },
  })
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return handleRequest(request, env)
  },
} satisfies ExportedHandler<Env>
