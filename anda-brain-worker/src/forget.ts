import { parseElementId, type Session } from '@ldclabs/kip-do'

export interface ForgetInput { entities: string[]; dry_run?: boolean }
export interface ForgetReport {
  dry_run: boolean
  deleted_concepts: number; deleted_propositions: number; deleted_assertions: number
  deleted_evidence: number; deleted_activities: number
  entities: { entity: string; existed: boolean; error?: string }[]
}

/** Explicit administrative graph erasure, retaining the native per-target gate. */
export function forget(session: Session, input: ForgetInput, afterErase?: (ids: ReadonlySet<string>) => void): ForgetReport {
  validateForget(input)
  const report: ForgetReport = {dry_run:input.dry_run ?? false,deleted_concepts:0,deleted_propositions:0,
    deleted_assertions:0,deleted_evidence:0,deleted_activities:0,entities:[]}
  const counters = {concept:'deleted_concepts',proposition:'deleted_propositions',assertion:'deleted_assertions',
    evidence:'deleted_evidence',activity:'deleted_activities'} as const
  const erased = new Set<string>()
  for (const entity of new Set(input.entities.map(id => id.trim()))) {
    const entry: ForgetReport['entities'][number] = {entity,existed:false}
    try {
      const id = parseElementId(entity)
      // KQL omits archived and tombstoned elements unless their state is
      // selected explicitly. Erasure must find those identity stubs as well.
      const row = session.nexus.store.load(id)
      entry.existed = row !== null && row.row.space === session.nexus.space &&
        row.row.state !== 'purged' && row.row.state !== 'pending' &&
        session.effectiveAuthority().mayRead(row, session.auth) !== null
      if (entry.existed && !report.dry_run) {
        const outcome = session.execute('PURGE :id REFERENCE POLICY "authorized_cascade" CONFIRM "PURGE"',{id:entity})
        for (const change of outcome.changes) {
          if (change.op === 'purge' && change.kind in counters) report[counters[change.kind as keyof typeof counters]] += 1
        }
        for (const change of outcome.changes) if (change.op === 'purge') erased.add(change.id)
      }
    } catch (error) { entry.error = error instanceof Error ? error.message : 'erasure failed' }
    report.entities.push(entry)
  }
  if (erased.size) afterErase?.(erased)
  return report
}
export function validateForget(value: unknown): asserts value is ForgetInput {
  if (!value || typeof value !== 'object' || !('entities' in value) || !Array.isArray(value.entities) ||
      value.entities.length > 100 || !value.entities.every(id => typeof id === 'string' && id.length <= 128) ||
      ('dry_run' in value && value.dry_run !== undefined && typeof value.dry_run !== 'boolean')) throw new Error('invalid forget request (max 100 element ids)')
}
