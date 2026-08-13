import { KipDatabase, type KipResponse } from '@ldclabs/kip-do'
import {
  assertFormationCommands,
  assertMaintenanceCommands,
  assertReadonlyCommands,
} from './kip.js'
import type { BrainStats, Env } from './types.js'

const APP_BOOTSTRAP_KEY = '__anda_brain_worker_bootstrap_version'
const APP_BOOTSTRAP_VERSION = '1'
const ACTOR_BOOTSTRAP = `
UPSERT {
  CONCEPT ?self { {type: "Person", name: "$self"} }
  CONCEPT ?system { {type: "Person", name: "$system"} }
}
`

/** One compact Anda Brain graph per Durable Object / space id. */
export class AndaBrain extends KipDatabase<Env> {
  #initializing?: Promise<void>

  override async executeKip(command: string): Promise<KipResponse> {
    await this.ensureInitialized()
    return super.executeKip(command)
  }

  override async executeKipBatch(commands: string[]): Promise<KipResponse[]> {
    await this.ensureInitialized()
    return super.executeKipBatch(commands)
  }

  async executeKipReadonlyBatch(commands: string[]): Promise<KipResponse[]> {
    assertReadonlyCommands(commands)
    await this.ensureInitialized()
    return super.executeKipBatch(commands)
  }

  async executeFormationPlan(commands: string[]): Promise<KipResponse[]> {
    assertFormationCommands(commands)
    await this.ensureInitialized()
    return super.executeKipBatch(commands)
  }

  async executeMaintenancePlan(commands: string[]): Promise<KipResponse[]> {
    assertMaintenanceCommands(commands)
    await this.ensureInitialized()
    return super.executeKipBatch(commands)
  }

  async describePrimer(): Promise<KipResponse> {
    await this.ensureInitialized()
    return super.executeKip('DESCRIBE PRIMER')
  }

  async stats(): Promise<BrainStats> {
    await this.ensureInitialized()
    const concepts = this.ctx.storage.sql
      .exec<{ count: number }>('SELECT COUNT(*) AS count FROM concepts')
      .one().count
    const propositions = this.ctx.storage.sql
      .exec<{ count: number }>('SELECT COUNT(*) AS count FROM proposition_links')
      .one().count

    return {
      concepts,
      propositions,
      initialized_at:
        this.ctx.storage.kv.get<string>(`${APP_BOOTSTRAP_KEY}:at`) ?? '',
      engine: '@ldclabs/kip-do',
    }
  }

  private ensureInitialized(): Promise<void> {
    if (this.ctx.storage.kv.get<string>(APP_BOOTSTRAP_KEY) === APP_BOOTSTRAP_VERSION) {
      return Promise.resolve()
    }
    if (this.#initializing) return this.#initializing

    this.#initializing = this.initializeActors().catch((error: unknown) => {
      this.#initializing = undefined
      throw error
    })
    return this.#initializing
  }

  private async initializeActors(): Promise<void> {
    const response = await super.executeKip(ACTOR_BOOTSTRAP)
    if ('error' in response) {
      throw new Error(`failed to initialize brain actors: ${response.error.message}`)
    }
    this.ctx.storage.kv.put(APP_BOOTSTRAP_KEY, APP_BOOTSTRAP_VERSION)
    this.ctx.storage.kv.put(`${APP_BOOTSTRAP_KEY}:at`, new Date().toISOString())
  }
}
