/**
 * Attention recall (KIP Memory Interface §4), mapped onto this Worker's API.
 *
 * Every item is raised by a commit: a `watch_fire` Activity for a fired Watch,
 * or a `commitment_review` Activity raising one due Commitment. `raised_seq`
 * is that Activity's `space_seq`. Items are ordered by `(raised_seq, ref)` and
 * the cursor is the last delivered position, so a page may stop inside one
 * commit and the next page continues after that item. Reading is read-only:
 * the host keeps the cursor, and consuming an item changes nothing in memory.
 * An item grants nothing. Mirrors `anda_brain/src/space/attention_recall.rs`.
 */
import type { Json, JsonMap, KipResult } from '@ldclabs/kip-do'

const CURSOR_PREFIX = 'attention:'
const CURSOR_START = 'attention:start'
const MAX_PAGE = 50
/** The most raising Activities one page reads. */
const ACTIVITY_WINDOW = 200
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

/** A position in the attention order; a missing ref covers the whole `seq`. */
export interface AttentionPosition { seq: number; ref?: string }

/**
 * `attention:start` (or the older `attention:-1`) is before every item;
 * `attention:<seq>` is after every item raised at `seq`; `attention:<seq>:<ref>`
 * is after that one item.
 */
export function parseAttentionCursor(cursor: string | undefined): AttentionPosition | undefined {
  if (cursor === undefined) return undefined
  const value = cursor.startsWith(CURSOR_PREFIX) ? cursor.slice(CURSOR_PREFIX.length) : ''
  if (value === 'start' || value === '-1') return undefined
  const match = /^(\d{1,15})(?::(.+))?$/.exec(value)
  if (!match) throw new Error('invalid attention cursor')
  return match[2] === undefined ? { seq: Number(match[1]) } : { seq: Number(match[1]), ref: match[2] }
}

/** Whether an item at `(seq, ref)` comes after `after`. */
function later(after: AttentionPosition | undefined, seq: number, ref: string): boolean {
  if (after === undefined) return true
  return seq > after.seq || (seq === after.seq && after.ref !== undefined && ref > after.ref)
}

export function recallAttention(read: ReadKip, input: AttentionRecallInput): AttentionRecall {
  const after = parseAttentionCursor(input.attention_cursor)
  const limit = input.limit ?? 20
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) {
    throw new Error(`attention limit must be 1..=${MAX_PAGE}`)
  }
  // A cursor inside a commit reads that commit again and skips what was
  // delivered; a whole-commit cursor starts at the next one.
  const from = after === undefined ? 0 : after.ref === undefined ? after.seq + 1 : after.seq
  const rows = rowsOf(read(`FIND(?a.id, ?a._system.space_seq, ?a.activity_class) WHERE {
  ?a ACTIVITY {}
  FILTER((?a.activity_class == "watch_fire" || ?a.activity_class == "commitment_review") && ?a._system.space_seq >= :from)
} ORDER BY ?a._system.space_seq ASC LIMIT :limit`, { from, limit: ACTIVITY_WINDOW }))
  let raised = rows.flatMap((row) => Array.isArray(row) && typeof row[0] === 'string' &&
    typeof row[1] === 'number' && typeof row[2] === 'string' ? [{ id: row[0], seq: row[1], kind: row[2] }] : [])
  // A full window may have cut its last commit in two; leave that commit to
  // the next page so each commit is read whole.
  const windowFull = raised.length >= ACTIVITY_WINDOW
  if (windowFull && raised[0]!.seq !== raised[raised.length - 1]!.seq) {
    const last = raised[raised.length - 1]!.seq
    raised = raised.filter((activity) => activity.seq !== last)
  }

  const items: AttentionItem[] = []
  let more = false
  let scannedThrough: number | undefined
  let index = 0
  commits: while (index < raised.length) {
    const seq = raised[index]!.seq
    const commit: AttentionItem[] = []
    while (index < raised.length && raised[index]!.seq === seq) {
      commit.push(...raisedItems(read, raised[index]!.id, seq, raised[index]!.kind))
      index += 1
    }
    commit.sort((a, b) => (a.ref < b.ref ? -1 : a.ref > b.ref ? 1 : 0))
    for (const [position, item] of commit.entries()) {
      if (position > 0 && commit[position - 1]!.ref === item.ref) continue
      if (!later(after, seq, item.ref)) continue
      if (items.length === limit) {
        more = true
        break commits
      }
      items.push(item)
    }
    scannedThrough = seq
  }
  const last = items[items.length - 1]
  const attention_cursor = more && last
    ? `${CURSOR_PREFIX}${last.raised_seq}:${last.ref}`
    : scannedThrough !== undefined ? `${CURSOR_PREFIX}${scannedThrough}`
      : input.attention_cursor ?? CURSOR_START
  return { items, attention_cursor, complete: !more && !windowFull }
}

/** The items one raising Activity carries. */
function raisedItems(read: ReadKip, activity: string, seq: number, kind: string): AttentionItem[] {
  const inputs = rowsOf(read(`FIND(?x.id, ?x.schema_ref, ?x.attributes) WHERE {
  ?a ACTIVITY {id: :activity}
  STRUCTURAL (?a, "inputs", ?x)
  ?x CONCEPT {}
} LIMIT :limit`, { activity, limit: MAX_REVIEW_TARGETS }))
  const items: AttentionItem[] = []
  for (const input of inputs) {
    if (!Array.isArray(input) || typeof input[0] !== 'string' || typeof input[1] !== 'string') continue
    const [id, schemaRef] = [input[0], input[1]]
    const attributes = isMap(input[2]) ? input[2] : {}
    if (kind === 'watch_fire' && schemaRef === `${PROFILE}Watch`) {
      items.push({ ref: id, kind: 'watch_fired', summary: summary(attributes, 'Watch fired'),
        raised_seq: seq, target_refs: watchedTargets(read, id) })
    } else if (kind === 'commitment_review' && schemaRef === `${PROFILE}Commitment`) {
      items.push({ ref: id, kind: 'commitment_due', summary: summary(attributes, 'Commitment due'),
        raised_seq: seq,
        ...(typeof attributes.due_at === 'string' ? { due_at: attributes.due_at } : {}),
        target_refs: [id],
        ...(typeof attributes.priority === 'number' ? { priority: attributes.priority } : {}) })
    }
  }
  return items
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
