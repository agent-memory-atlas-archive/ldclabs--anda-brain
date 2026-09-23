import type { CorrectionCursor, CorrectionScan } from './types.js'
import type { KipOperation } from './kip.js'
import { processingError } from './processing.js'

export const CORRECTION_CURSOR_KEY = 'anda-brain:correction_cursor'
export const REVISED_ROOTS_KEY = 'anda-brain:revised_roots'
const PENDING = 'anda-brain:pending_corrections'
const RUN = 'anda-brain:maintenance_run'
interface MaintenanceRun { id: string; epoch: number; expires_at: number }

export function clearMaintenanceContext(kv: SyncKvStorage): void {
  kv.delete(PENDING)
  kv.delete(REVISED_ROOTS_KEY)
}

/** One admitted maintenance call and one durable, unacknowledged correction page. */
export class MaintenanceWork {
  constructor(private storage: DurableObjectStorage) {}
  begin(epoch: number, expiresAt: number): string {
    const old = this.storage.kv.get<MaintenanceRun>(RUN)
    if (old && old.epoch === epoch && old.expires_at > Date.now()) processingError('maintenance_busy')
    if (!Number.isFinite(expiresAt) || expiresAt <= Date.now() || expiresAt > Date.now() + 300_000) {
      processingError('maintenance_run_expired')
    }
    const id = crypto.randomUUID()
    this.storage.kv.put(RUN, { id, epoch, expires_at: expiresAt } satisfies MaintenanceRun)
    return id
  }
  check(id: string, epoch: number): void {
    const run = this.storage.kv.get<MaintenanceRun>(RUN)
    if (!run || run.id !== id || run.epoch !== epoch || run.expires_at <= Date.now()) {
      processingError('maintenance_run_expired')
    }
  }
  release(id: string): void {
    if (this.storage.kv.get<MaintenanceRun>(RUN)?.id === id) this.storage.kv.delete(RUN)
  }
  pending(): CorrectionScan | undefined { return this.storage.kv.get<CorrectionScan>(PENDING) }
  cursor(): number | CorrectionCursor { return this.storage.kv.get<number | CorrectionCursor>(CORRECTION_CURSOR_KEY) ?? 0 }
  save(page: CorrectionScan): void {
    if (!page.error) {
      if (page.revised_roots.length) this.storage.kv.put(PENDING, page)
      else {
        this.storage.kv.delete(PENDING)
        this.storage.kv.put(CORRECTION_CURSOR_KEY, {seq:page.cursor,after_id:page.cursor_after_id ?? ''})
      }
    }
    this.storage.kv.put(REVISED_ROOTS_KEY, page.revised_roots)
  }
  validateAcknowledgement(ids: readonly string[]): void {
    const roots = this.pending()?.revised_roots ?? []
    if (ids.some(id => !roots.some(root => root.assertion === id))) {
      throw new Error('invalid_correction_acknowledgement')
    }
  }
  acknowledge(ids: readonly string[]): void {
    this.validateAcknowledgement(ids)
    const page = this.pending()
    if (!page) return
    const remaining = page.revised_roots.filter(root => !ids.includes(root.assertion))
    if (remaining.length) {
      const pending = { ...page, revised_roots: remaining }
      this.storage.kv.put(PENDING, pending)
      this.storage.kv.put(REVISED_ROOTS_KEY, remaining)
      return
    }
    this.storage.kv.put(CORRECTION_CURSOR_KEY, { seq: page.cursor, after_id: page.cursor_after_id ?? '' })
    clearMaintenanceContext(this.storage.kv)
  }

  /** SQL only selects candidate ids; native KQL still governs and renders each.
   * Rotation means last offered, never proof of processing or change coverage. */
  snapshot(space: string): KipOperation[] {
    const groups = [
      ['events', "lineage = 'kip://profiles/cognitive-memory/Event'"],
      ['tasks', "lineage = 'kip://profiles/cognitive-memory/SleepTask' AND json_extract(attributes, '$.status') IN ('pending', 'running', 'blocked')"],
      ['memories', "lineage NOT IN ('kip://profiles/cognitive-memory/Event', 'kip://profiles/cognitive-memory/SleepTask', 'kip://profiles/cognitive-memory/Watch')"],
      ['watches', "lineage = 'kip://profiles/cognitive-memory/Watch' AND json_extract(attributes, '$.status') = 'disarmed'"],
    ] as const
    return groups.map(([name, selection]) => {
      const key = `anda-brain:maintenance_snapshot:${name}`
      const select = (after: number) => this.storage.sql.exec<{ id: number }>(
        `SELECT id FROM concepts WHERE space = ? AND state = 'active' AND id > ? AND ${selection} ORDER BY id LIMIT 20`,
        space, after,
      ).toArray()
      let ids = select(this.storage.kv.get<number>(key) ?? 0)
      if (!ids.length) ids = select(0)
      if (!ids.length) return { command: 'FIND(?item) WHERE { ?item CONCEPT {id: "C-1"} FILTER(1 == 0) } LIMIT 20' }
      this.storage.kv.put(key, ids.at(-1)!.id)
      const clauses = ids.map((_, index) => `?item CONCEPT {id: :id${index}}`)
      return { command: `FIND(?item) WHERE { ${clauses[0]} ${clauses.slice(1).map(clause => `UNION { ${clause} }`).join(' ')} } LIMIT 20`,
        parameters: Object.fromEntries(ids.map(({id}, index) => [`id${index}`, `C-${id}`])) }
    })
  }
}
