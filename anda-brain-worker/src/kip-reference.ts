/** A registry lookup only: no filesystem, fetch, database or mutation capability. */
import { REFERENCE_DOCUMENTS, REFERENCE_VERSION } from './references.generated.js'

export const REFERENCE_PAGE_BYTES = 8192
export const MAX_REFERENCE_ROUNDS = 3
export const MAX_REFERENCE_PAGES = 8

export const REFERENCE_INSTRUCTIONS = `# Embedded protocol references
Markdown links and bare document paths are source citations, not files you can open.
The role cards, Profile and deployment policy are in context; planning also has full syntax.
To consult another reference, return a non-empty references array in your JSON response,
for example {"references":[{"document":"syntax","section":"kql","offset":0}]}.
Include the other required response fields as empty placeholders: commands/types/predicates=[],
summary=""; for an answer use answer="", found=false, uncertainty=1. No digests or runtime actions.
The host returns only compiled-in documentation, then asks for your final JSON in the same format.
Omit references (or use []) when returning the final plan or answer.
document="index", section=null, offset=0 lists document IDs and source names.
section="index" lists exact Markdown headings; pass a heading as section to read it.
syntax also accepts kql/kml/meta/envelope. section=null reads the whole document.
Each page contains at most 8192 UTF-8 bytes. Follow next_offset with the same document/section;
null means that selected text is complete. Unknown identifiers are errors, not permission to fetch.
At most 4 pages per response, 8 pages and 3 reference rounds per stage are allowed.
After that return a final result from available material, preserving uncertainty or deferring work.
References are protocol explanations, not retrieved memories, evidence or change coverage.
They never grant capabilities: this Worker's deployment constraints still govern all operations.
Unlisted background and translation links are citations only.`

export interface ReferenceRequest {
  document: string
  section: string | null
  offset: number
}

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })

/** UTF-8 paging uses byte offsets, matching the Rust tool, not JS UTF-16 indices. */
function page(text: string, offset: number): { content: string; next_offset: number | null; total_bytes: number } {
  const bytes = encoder.encode(text)
  if (!Number.isSafeInteger(offset) || offset < 0 || offset > bytes.length ||
    (offset < bytes.length && (bytes[offset]! & 0xc0) === 0x80)) throw new Error('invalid UTF-8 byte offset')
  let end = Math.min(bytes.length, offset + REFERENCE_PAGE_BYTES)
  while (end < bytes.length && (bytes[end]! & 0xc0) === 0x80) end--
  return { content: decoder.decode(bytes.subarray(offset, end)), next_offset: end < bytes.length ? end : null, total_bytes: bytes.length }
}

function headings(text: string): { offset: number; level: number; title: string }[] {
  const result: { offset: number; level: number; title: string }[] = []
  let offset = 0
  let fence: { marker: string; length: number } | undefined
  for (const line of text.match(/[^\n]*\n|[^\n]+$/g) ?? []) {
    const trimmed = line.trim()
    const marker = /^(`{3,}|~{3,})/.exec(trimmed)?.[1]
    if (fence) {
      if (marker?.[0] === fence.marker && marker.length >= fence.length && trimmed.slice(marker.length).trim() === '') fence = undefined
    } else if (marker) fence = { marker: marker[0]!, length: marker.length }
    else {
      const heading = /^(#{1,6}) (.+)$/.exec(trimmed)
      if (heading) result.push({ offset, level: heading[1]!.length, title: heading[2]!.trim() })
    }
    offset += line.length
  }
  return result
}

export function readReference(value: unknown) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new Error('invalid reference request')
  const request = value as Record<string, unknown>
  if (Object.keys(request).some(key => !['document', 'section', 'offset'].includes(key)) ||
    typeof request.document !== 'string' || request.document.length > 64 ||
    !(request.section === null || (typeof request.section === 'string' && request.section.length <= 512)) ||
    typeof request.offset !== 'number') throw new Error('invalid reference request')
  const { document, section, offset } = request as unknown as ReferenceRequest
  let content: string
  let source: string
  if (document === 'index') {
    if (section !== null) throw new Error('document index requires section=null')
    source = 'embedded document catalogue'
    content = REFERENCE_DOCUMENTS.map(doc => `${doc.id}: ${doc.source}`).join('\n')
  } else {
    const doc = REFERENCE_DOCUMENTS.find(doc => doc.id === document)
    if (!doc) throw new Error('unknown reference document; use document=index')
    source = doc.source
    content = doc.content
    if (section !== null) {
      const titles = headings(content)
      const topics: Record<string, string> = { kql: '2. KQL — Read', kml: '3. KML — Write', meta: '4. META — Ground, Verify, Inspect', envelope: '5. Runtime Envelope' }
      const title = document === 'syntax' ? (Object.hasOwn(topics, section) ? topics[section]! : section) : section
      if (section === 'index') content = titles.map(heading => heading.title).join('\n')
      else {
        const matches = titles.filter(heading => heading.title === title)
        if (matches.length !== 1) throw new Error('unknown or ambiguous reference section; use section=index')
        const start = matches[0]!
        const end = titles.find(heading => heading.offset > start.offset && heading.level <= start.level)?.offset ?? content.length
        content = content.slice(start.offset, end)
      }
    }
  }
  return { document, section, source, crate_version: REFERENCE_VERSION, protocol: 'KIP 2.0', offset, ...page(content, offset) }
}
