// KIP conformance harness adapter for the Anda Brain Memory Interface binding.
//
// An engine adapter: every exercise drives a real `anda_brain` process over its
// HTTP binding (`POST /v1/{space_id}/memory` and the receipt/source helpers).
// Each fixture gets a fresh process and database, so a scenario can restart the
// host. The process talks to a controlled model served here: every completion
// answers "done" (Formation completes and forms nothing), and a completion whose
// prompt carries a hold marker waits until the scenario releases it. No model
// provider is contacted and nothing is billed.
//
// Implemented: the scenarios whose observations do not depend on what a model
// extracts — KIP2-MIF-002, KIP2-MIF-005 and KIP2-REL-013. Every other scenario
// needs a live model provider and is refused as not run; see README.md.
//
//   ANDA_BRAIN_BIN=/abs/path/target/debug/anda_brain node run-deterministic.mjs
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import http from 'node:http'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'

const SPACE = 'conformance'
const OTHER_SPACE = 'conformance_other'

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer()
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address()
      server.close(() => resolve(port))
    })
  })
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/** An OpenAI-compatible chat completion endpoint that always answers "done". */
class ControlledModel {
  constructor() {
    this.held = new Map()
    this.calls = 0
  }

  async start() {
    this.server = http.createServer((req, res) => {
      let body = ''
      req.on('data', (chunk) => (body += chunk))
      req.on('end', async () => {
        this.calls += 1
        for (const [marker, gate] of this.held) {
          if (body.includes(marker)) await gate.promise
        }
        res.setHeader('content-type', 'application/json')
        res.end(
          JSON.stringify({
            id: `controlled-${this.calls}`,
            object: 'chat.completion',
            created: Math.floor(Date.now() / 1000),
            model: 'controlled',
            choices: [
              { index: 0, message: { role: 'assistant', content: 'done' }, finish_reason: 'stop' }
            ],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }
          })
        )
      })
    })
    const port = await freePort()
    await new Promise((resolve) => this.server.listen(port, '127.0.0.1', resolve))
    this.url = `http://127.0.0.1:${port}/v1`
  }

  hold(marker) {
    let release
    const promise = new Promise((resolve) => (release = resolve))
    this.held.set(marker, { promise, release })
  }

  release(marker) {
    this.held.get(marker)?.release()
    this.held.delete(marker)
  }

  async stop() {
    for (const marker of [...this.held.keys()]) this.release(marker)
    this.server.closeAllConnections?.()
    await new Promise((resolve) => this.server.close(resolve))
  }
}

/** One `anda_brain` process over one database directory. */
class BrainProcess {
  constructor(dir, model) {
    this.dir = dir
    this.model = model
  }

  async start() {
    const bin = process.env.ANDA_BRAIN_BIN
    if (!bin) throw new Error('set ANDA_BRAIN_BIN to an anda_brain binary built with --features mcp,wiki')
    this.port = await freePort()
    this.base = `http://127.0.0.1:${this.port}`
    await mkdir(path.join(this.dir, 'db'), { recursive: true })
    this.child = spawn(bin, ['local', '--db', path.join(this.dir, 'db')], {
      // Run where no .env exists, with only what the process needs.
      cwd: this.dir,
      env: {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        LOG_LEVEL: 'warn',
        LISTEN_ADDR: `127.0.0.1:${this.port}`,
        ED25519_PUBKEYS: '',
        MODEL_FAMILY: 'openai',
        MODEL_NAME: 'controlled',
        MODEL_API_KEY: 'controlled',
        MODEL_API_BASE: this.model.url,
        MCP_HTTP_ENABLED: 'false'
      },
      stdio: ['ignore', 'ignore', 'inherit']
    })
    let exited = false
    this.exited = new Promise((resolve) =>
      this.child.once('exit', (code) => {
        exited = true
        resolve(code)
      })
    )
    for (let i = 0; i < 200 && !exited; i++) {
      try {
        const res = await fetch(`${this.base}/info`)
        if (res.ok) return
      } catch {}
      await sleep(100)
    }
    throw new Error('anda_brain did not start')
  }

  async stop() {
    if (!this.child) return
    this.child.kill('SIGTERM')
    await this.exited
    this.child = null
  }

  async restart() {
    await this.stop()
    await this.start()
  }

  async call(method, route, body) {
    const res = await fetch(`${this.base}${route}`, {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body)
    })
    const text = await res.text()
    try {
      return JSON.parse(text)
    } catch {
      throw new Error(`${method} ${route}: ${res.status} ${text.slice(0, 300)}`)
    }
  }

  async createSpace(space) {
    const out = await this.call('POST', '/admin/create_space', {
      user: 'aaaaa-aa',
      space_id: space,
      tier: 1
    })
    if (out.error) throw new Error(`create_space ${space}: ${JSON.stringify(out.error)}`)
  }

  /** One Memory Interface request; the response is kept as raw evidence. */
  async memory(space, request, raw) {
    const response = await this.call('POST', `/v1/${space}/memory`, { kip_memory: '2.0', ...request })
    raw.push({ request, response })
    return response
  }

  async stage(space, text, key, raw) {
    const out = await this.call('POST', `/v1/${space}/memory/sources`, {
      messages: [{ role: 'user', content: [{ type: 'Text', text }] }],
      observed_at: '2026-09-01T00:00:00.000Z',
      idempotency_key: key
    })
    raw.push({ stage: key, response: out })
    if (out.error) throw new Error(`stage ${key}: ${JSON.stringify(out.error)}`)
    return out.result
  }

  async progress(space, receipt, raw) {
    const out = await this.call('GET', `/v1/${space}/memory/receipts/${receipt}`)
    raw.push({ receipt, response: out })
    if (out.error) throw new Error(`receipt ${receipt}: ${JSON.stringify(out.error)}`)
    return out.result.progress
  }

  async waitProcessed(space, receipt, raw) {
    for (let i = 0; i < 600; i++) {
      const progress = await this.progress(space, receipt, [])
      if (progress.phase !== 'recorded') {
        raw.push({ receipt, settled: progress })
        return progress
      }
      await sleep(100)
    }
    throw new Error(`receipt ${receipt} stayed recorded`)
  }
}

/** Host-side receipt bookkeeping with the MemorySession semantics (MI §5.1). */
class Session {
  constructor(snapshot = { outstanding: [] }) {
    this.outstanding = [...snapshot.outstanding]
  }
  record(receipt) {
    if (!this.outstanding.includes(receipt)) this.outstanding.push(receipt)
  }
  acknowledge(accounted) {
    this.outstanding = this.outstanding.filter((r) => !accounted.includes(r))
  }
  snapshot() {
    return { outstanding: [...this.outstanding] }
  }
}

const attention = (after, deadline_ms = 300) => ({
  operation: 'recall',
  budget: { max_output_tokens: 4096, deadline_ms },
  input: { mode: 'attention', ...(after.length ? { after } : {}) }
})

const errorCode = (response) => response?.error?.code ?? null

const scenarios = {
  // Persisted input is not processed memory.
  async 'KIP2-MIF-002'(brain, model, raw) {
    const marker = 'HOLD-MIF-002'
    model.hold(marker)
    const source = await brain.stage(SPACE, `${marker}: I prefer green tea.`, 'mif-002', raw)
    const observed = await brain.memory(
      SPACE,
      { operation: 'observe', idempotency_key: 'mif-002', input: { source_ref: source.source_ref } },
      raw
    )
    const receipt = observed.receipt.receipt_ref
    const before = await brain.memory(SPACE, attention([receipt]), raw)
    model.release(marker)
    await brain.waitProcessed(SPACE, receipt, raw)
    const after = await brain.memory(SPACE, attention([receipt]), raw)
    const replay = await brain.memory(
      SPACE,
      { operation: 'observe', idempotency_key: 'mif-002', input: { source_ref: source.source_ref } },
      raw
    )
    return {
      observed: {
        before_formation_satisfied: before.result.coverage.pending_receipts.length === 0,
        before_formation_action_eligible: before.result.coverage.action_eligible,
        after_formation_satisfied: after.result.coverage.pending_receipts.length === 0
      },
      state: { intake_count: new Set([receipt, replay.receipt.receipt_ref]).size }
    }
  },

  // Intake retries survive restart.
  async 'KIP2-MIF-005'(brain, _model, raw) {
    const source = await brain.stage(SPACE, 'The deploy window is Friday.', 'mif-005', raw)
    const observe = (extra = {}) =>
      brain.memory(
        SPACE,
        { operation: 'observe', idempotency_key: 'mif-005', input: { source_ref: source.source_ref }, ...extra },
        raw
      )
    const first = await observe({ request_id: 'transport-1' })
    const acks = [first.receipt]
    // After durable intake, and again after formation.
    await brain.restart()
    acks.push((await observe({ request_id: 'transport-2', budget: { max_output_tokens: 300, deadline_ms: 1000 } })).receipt)
    await brain.waitProcessed(SPACE, first.receipt.receipt_ref, raw)
    await brain.restart()
    acks.push((await observe({ request_id: 'transport-3' })).receipt)
    const other = await brain.stage(SPACE, 'The deploy window is Monday.', 'mif-005-other', raw)
    const changedSource = await brain.memory(
      SPACE,
      { operation: 'observe', idempotency_key: 'mif-005', input: { source_ref: other.source_ref } },
      raw
    )
    const changedScope = await observe({ scope: { task_ref: 'task-9' } })
    const info = await brain.call('GET', `/v1/${SPACE}/info`)
    raw.push({ info })
    return {
      observed: {
        same_acknowledgement: acks.every((ack) => JSON.stringify(ack) === JSON.stringify(first.receipt)),
        changed_source_error: errorCode(changedSource),
        changed_scope_error: errorCode(changedScope)
      },
      state: {
        intake_count: new Set(acks.map((ack) => ack.receipt_ref)).size,
        formation_count: info.result.conversations
      }
    }
  },

  // Session barrier retention.
  async 'KIP2-REL-013'(brain, model, raw) {
    const marker = 'HOLD-REL-013'
    const a = await brain.stage(SPACE, `${marker}: A — the office moved to Pier 9.`, 'rel-013-a', raw)
    const b = await brain.stage(SPACE, 'B — the standup is at 10.', 'rel-013-b', raw)
    const session = new Session()
    // B is formed first; A stays in Formation behind a hold.
    const observedB = await brain.memory(
      SPACE,
      { operation: 'observe', idempotency_key: 'rel-013-b', input: { source_ref: b.source_ref } },
      raw
    )
    session.record(observedB.receipt.receipt_ref)
    await brain.waitProcessed(SPACE, observedB.receipt.receipt_ref, raw)
    model.hold(marker)
    const observedA = await brain.memory(
      SPACE,
      { operation: 'observe', idempotency_key: 'rel-013-a', input: { source_ref: a.source_ref } },
      raw
    )
    session.record(observedA.receipt.receipt_ref)
    const issued = [observedA.receipt.receipt_ref, observedB.receipt.receipt_ref]
    // Checkpoint and restore the host session; recall without a model-written after.
    const restored = new Session(JSON.parse(JSON.stringify(session.snapshot())))
    const recall = await brain.memory(SPACE, attention(restored.outstanding), raw)
    const pending = recall.result.coverage.pending_receipts
    restored.acknowledge(recall.result.after.filter((p) => p.phase !== 'recorded').map((p) => p.receipt_ref))
    // A receipt of another Space is existence-neutral here.
    const foreign = await brain.stage(OTHER_SPACE, 'C — elsewhere.', 'rel-013-c', raw)
    const foreignObserved = await brain.memory(
      OTHER_SPACE,
      { operation: 'observe', idempotency_key: 'rel-013-c', input: { source_ref: foreign.source_ref } },
      raw
    )
    const foreignRecall = await brain.memory(SPACE, attention([foreignObserved.receipt.receipt_ref]), raw)
    model.release(marker)
    const accounted = issued.filter((r) => !restored.outstanding.includes(r))
    const readable = await Promise.all(issued.map((r) => brain.progress(SPACE, r, raw).then(() => true, () => false)))
    return {
      observed: {
        pending_A_preserved:
          pending.includes(observedA.receipt.receipt_ref) &&
          restored.outstanding.includes(observedA.receipt.receipt_ref),
        foreign_receipt_rejected: errorCode(foreignRecall) === 'NotFoundOrNotVisible'
      },
      state: {
        lost_receipts: issued.filter(
          (r, i) => !readable[i] || (!restored.outstanding.includes(r) && !accounted.includes(r))
        ).length
      }
    }
  }
}

let model = null
let brain = null
let dir = null
let state = {}

async function teardown() {
  await brain?.stop()
  await model?.stop()
  if (dir) await rm(dir, { recursive: true, force: true })
  brain = model = dir = null
}

async function launch() {
  await teardown()
  dir = await mkdtemp(path.join(os.tmpdir(), 'anda-brain-conformance-'))
  model = new ControlledModel()
  await model.start()
  brain = new BrainProcess(dir, model)
  await brain.start()
  await brain.createSpace(SPACE)
  await brain.createSpace(OTHER_SPACE)
}

export default {
  async describe() {
    await launch()
    const info = await brain.call('GET', '/info')
    const described = await brain.call('POST', `/v1/${SPACE}/execute_kip_readonly`, {
      command: 'DESCRIBE CAPABILITIES'
    })
    const registry = described.results?.[0]?.result?.supported?.registry ?? {}
    await teardown()
    const capabilities = []
    if (info.memory_interface?.bundles?.includes('memory_basic')) capabilities.push('memory_interface')
    if (registry.recording_repair === true) capabilities.push('recording_repair')
    return { kind: 'engine', name: 'anda_brain', version: info.version, capabilities }
  },

  async seed(fixture) {
    if (fixture?.reset === false && brain) return
    await launch()
    state = {}
  },

  async harness(action, args) {
    if (!['exercise_memory_interface_scenario', 'exercise_memory_reliability_scenario'].includes(action)) {
      throw new Error(`unsupported harness action ${action}`)
    }
    const run = scenarios[args.id]
    if (!run) {
      throw new Error(`${args.id} depends on what a model extracts; it runs only against a live model provider and was not run`)
    }
    const raw = []
    try {
      const outcome = await run(brain, model, raw)
      state = outcome.state
      return { observed: outcome.observed, raw_responses: raw }
    } catch (error) {
      await teardown()
      throw error
    }
  },

  async inspect() {
    const current = state
    await teardown()
    return current
  }
}
