/**
 * Attention recall (KIP Memory Interface §4), mapped onto this Worker's API.
 *
 * Every item is raised by a commit: a `watch_fire` Activity for a fired Watch,
 * or a `commitment_review` Activity in which Maintenance recorded the
 * Commitments it found due. `raised_seq` is that Activity's `space_seq`, which
 * is what the cursor orders by. Reading is read-only: the host keeps the
 * cursor, and consuming an item changes nothing in memory. An item grants
 * nothing. Mirrors `anda_brain/src/space/attention_recall.rs`.
 */
import type { Json, JsonMap, KipResult } from '@ldclabs/kip-do'

const CURSOR_PREFIX = 'attention:'
const MAX_PAGE = 50
const MAX_REVIEW_TARGETS = 128
const PROFILE = 'kip://profiles/cognitive-memory@2.0.0/'

export interface AttentionRecallInput { attention_cursor?: string; limit?: number }
export interface AttentionItem {
  ref: string
  kind: 'watch_fired' | 'commitment_due'
  summary: string
  raised_seq: number
  due_at?: string
  target_refs: string[]
  priority?: number
}
export interface AttentionRecall { items: AttentionItem[]; attention_cursor: string; complete: boolean }

export type ReadKip = (command: string, parameters: JsonMap) => KipResult

/** `attention:-1` is before every raise; `attention:<n>` is after `raised_seq` n. */
export function parseAttentionCursor(cursor: string | undefined): number | undefined {
  if (cursor === undefined) return undefined
  const value = cursor.startsWith(CURSOR_PREFIX) ? cursor.slice(CURSOR_PREFIX.length) : ''
  if (value === '-1') return undefined
  if (!/^\d{1,15}$/.test(value)) throw new Error('invalid attention cursor')
  return Number(value)
}

export function recallAttention(read: ReadKip, input: AttentionRecallInput): AttentionRecall {
  const after = parseAttentionCursor(input.attention_cursor)
  const limit = input.limit ?? 20
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) {
    throw new Error(`attention limit must be 1..=${MAX_PAGE}`)
  }
  const rows = rowsOf(read(`FIND(?a.id, ?a._system.space_seq, ?a.activity_class) WHERE {
  ?a ACTIVITY {}
  FILTER((?a.activity_class == "watch_fire" || ?a.activity_class == "commitment_review") && ?a._system.space_seq > :after)
} ORDER BY ?a._system.space_seq ASC LIMIT :limit`, { after: after ?? -1, limit }))
  let raised = rows.flatMap((row) => Array.isArray(row) && typeof row[0] === 'string' &&
    typeof row[1] === 'number' && typeof row[2] === 'string' ? [{ id: row[0], seq: row[1], kind: row[2] }] : [])
  // A full page may have cut one commit's Activities in two. Stop one
  // coordinate short so the next page reads that commit whole.
  const complete = raised.length < limit
  if (!complete && raised.length > 0 && raised[0]!.seq !== raised[raised.length - 1]!.seq) {
    const last = raised[raised.length - 1]!.seq
    raised = raised.filter((activity) => activity.seq !== last)
  }

  const items: AttentionItem[] = []
  for (const activity of raised) {
    const inputs = rowsOf(read(`FIND(?x.id, ?x.schema_ref, ?x.attributes) WHERE {
  ?a ACTIVITY {id: :activity}
  STRUCTURAL (?a, "inputs", ?x)
  ?x CONCEPT {}
} LIMIT :limit`, { activity: activity.id, limit: MAX_REVIEW_TARGETS }))
    for (const input of inputs) {
      if (!Array.isArray(input) || typeof input[0] !== 'string' || typeof input[1] !== 'string') continue
      const [id, schemaRef] = [input[0], input[1]]
      const attributes = isMap(input[2]) ? input[2] : {}
      if (activity.kind === 'watch_fire' && schemaRef === `${PROFILE}Watch`) {
        items.push({ ref: id, kind: 'watch_fired', summary: summary(attributes, 'Watch fired'),
          raised_seq: activity.seq, target_refs: watchedTargets(read, id) })
      } else if (activity.kind === 'commitment_review' && schemaRef === `${PROFILE}Commitment`) {
        items.push({ ref: id, kind: 'commitment_due', summary: summary(attributes, 'Commitment due'),
          raised_seq: activity.seq,
          ...(typeof attributes.due_at === 'string' ? { due_at: attributes.due_at } : {}),
          target_refs: [id],
          ...(typeof attributes.priority === 'number' ? { priority: attributes.priority } : {}) })
      }
    }
  }
  const delivered = raised.length > 0 ? raised[raised.length - 1]!.seq : after
  return { items, attention_cursor: `${CURSOR_PREFIX}${delivered ?? -1}`, complete }
}

/** What a Watch is about: its `watches` targets. */
function watchedTargets(read: ReadKip, watch: string): string[] {
  return rowsOf(read(`FIND(?t.id) WHERE {
  ?w CONCEPT {id: :watch}
  STRUCTURAL (?w, "watches", ?t)
} LIMIT :limit`, { watch, limit: MAX_REVIEW_TARGETS })).filter((id): id is string => typeof id === 'string')
}

function rowsOf(result: KipResult): Json[] {
  if (result.status === 'failed') throw new Error(result.error?.message ?? 'attention read failed')
  return Array.isArray(result.result) ? result.result : []
}

function isMap(value: unknown): value is JsonMap {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function summary(attributes: JsonMap, fallback: string): string {
  const text = typeof attributes.summary === 'string' ? [...attributes.summary].slice(0, 4096).join('') : ''
  return text.trim() ? text : fallback
}
