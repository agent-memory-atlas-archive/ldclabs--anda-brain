#!/usr/bin/env node
/**
 * Inlines `assets/*.md` into TypeScript — GENERATED OUTPUT, COMMIT IT.
 *
 * A Worker is bundled by esbuild and a test run is bundled by Vite, and the two
 * disagree about what importing a `.md` file means. A generated module means
 * one import that both understand, and it keeps the prompts readable as
 * Markdown in the repository rather than as escaped strings in a source file.
 *
 * The Rust service does not need this: `anda_kip` ships the role and syntax
 * cards plus the Cognitive Memory Profile, and the runtime reads them directly.
 * `@ldclabs/kip-do` ships none of those assets, so this Worker vendors copies —
 * with the cost that copies drift. `pnpm run sync:assets` re-syncs every one of
 * them from `anda_kip`, including the reference half of each `Brain*.md`; run it
 * before this.
 */
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const root = dirname(here)

/** Each asset, with the constant it becomes and where it came from. */
const ASSETS = [
  ...[['KIPFormation.md', 'KIP_FORMATION_CARD'], ['KIPRecall.md', 'KIP_RECALL_CARD'],
    ['KIPMaintenance.md', 'KIP_MAINTENANCE_CARD'], ['MemoryInterface.md', 'MEMORY_AGENT_CARD']]
    .map(([file, constant]) => ({file, constant, origin: `anda-db/rs/anda_kip/brain/${file}`, doc: 'The upstream role card.'})),
  {
    file: 'KIPSyntax.md',
    constant: 'KIP_SYNTAX',
    origin: 'anda-db/rs/anda_kip/KIPSyntax.md',
    doc: 'The LLM-facing KIP 2.0 syntax card.',
  },
  {
    file: 'CognitiveMemoryProfile-2.0.md',
    constant: 'COGNITIVE_MEMORY_PROFILE',
    origin: 'anda-db/rs/anda_kip/profiles/CognitiveMemoryProfile-2.0.md',
    doc: 'The Cognitive Memory Profile, the ontology every mode assumes.',
  },
  {
    file: 'BrainFormation.md',
    constant: 'BRAIN_FORMATION',
    origin: 'anda-db/rs/anda_kip/brain/BrainFormation.md, plus section A',
    doc: 'The reference Formation policy and this deployment’s contract.',
  },
  {
    file: 'BrainRecall.md',
    constant: 'BRAIN_RECALL',
    origin: 'anda-db/rs/anda_kip/brain/BrainRecall.md, plus section A',
    doc: 'The reference Recall policy and this deployment’s contract.',
  },
  {
    file: 'BrainMaintenance.md',
    constant: 'BRAIN_MAINTENANCE',
    origin: 'anda-db/rs/anda_kip/brain/BrainMaintenance.md, plus section A',
    doc: 'The reference Maintenance policy and this deployment’s contract.',
  },
]

const bodies = ASSETS.map(({ file, constant, origin, doc }) => {
  const text = readFileSync(join(root, 'assets', file), 'utf8')
  if (text.trim() === '') throw new Error(`assets/${file} is empty`)
  return `/**
 * ${doc}
 *
 * Vendored from \`${origin}\`.
 */
export const ${constant}: string = ${JSON.stringify(text)}
`
})

const out = `/**
 * The agent prompts, inlined — GENERATED FILE, DO NOT EDIT.
 *
 * Source of truth: \`assets/*.md\`. Regenerate with \`pnpm run codegen:prompts\`.
 */

${bodies.join('\n')}`

const target = join(root, 'src', 'assets.generated.ts')
writeFileSync(target, out)
console.log(
  `wrote ${target} (${ASSETS.length} assets, ${out.length.toLocaleString()} chars)`,
)
