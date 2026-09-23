import {
  Session, KipError, parseKip, receiptOf,
  type EffectiveAuthority, type KipResult,
} from '@ldclabs/kip-do'
import type { IngestContext, KipOperation } from './kip.js'

/** Session-local visibility narrowing, before matching, joins or aggregation.
 * Native grants still decide authority; administrative sessions are untouched. */
export class CurrentMemorySession extends Session {
  override effectiveAuthority(space = this.nexus.space): EffectiveAuthority {
    const authority = super.effectiveAuthority(space)
    const mayRead = authority.mayRead.bind(authority)
    authority.mayRead = (element, auth) =>
      element.row.state === 'active' ? mayRead(element, auth) : null
    authority.readsWholeSpace = () => false
    return authority
  }
}

/** Preserve the engine's operation envelopes while using the narrowed Session. */
export function executeAgentOperations(
  session: CurrentMemorySession,
  operations: readonly KipOperation[],
  readonly: boolean,
  ingest?: IngestContext,
): KipResult[] {
  let stopped = false
  return operations.map(operation => {
    const identity = operation.op_id === undefined ? {} : { op_id: operation.op_id }
    if (stopped) return { ...identity, status: 'skipped' }
    try {
      const parsed = parseKip(operation.command)
      const params = operation.parameters ?? {}
      if ('Kml' in parsed) {
        if (readonly) throw new KipError('ReadonlyViolation', 'agent read cannot mutate memory')
        const outcome = session.mutate(parsed.Kml, params, { ingest })
        return { ...identity, status: outcome.status === 'no_effect' ? 'no_effect' : 'succeeded',
          receipt: receiptOf(outcome, session.auth),
          ...(outcome.warnings.length ? { warnings: outcome.warnings } : {}),
          extensions: { 'kip-do/outcome': outcome } }
      }
      if ('Kql' in parsed) {
        const answer = session.findPage(parsed.Kql, params)
        return { ...identity, status: 'succeeded', result: answer.rows,
          context: { snapshot_seq: answer.snapshotSeq, ...(answer.validAt === null ? {} : { valid_at: answer.validAt }) },
          ...(answer.nextCursor === null ? {} : { next_cursor: answer.nextCursor }) }
      }
      if (!readonly || !('Search' in parsed.Meta)) {
        throw new KipError('UnsupportedCapability', 'current agent metadata reads support SEARCH only')
      }
      const answer = session.describePage(operation.command, params)
      return { ...identity, status: 'succeeded', result: answer.result,
        ...(answer.nextCursor === null ? {} : { next_cursor: answer.nextCursor }) }
    } catch (error) {
      if (!readonly) stopped = true
      return { ...identity, status: 'failed', error: KipError.from(error).toJSON() }
    }
  })
}
