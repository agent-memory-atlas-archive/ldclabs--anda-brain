// Runs the Memory Interface scenarios this adapter exercises without a model
// provider, through the KIP repository's own vector runner.
//
//   KIP_ROOT=/abs/path/KIP ANDA_BRAIN_BIN=/abs/path/anda_brain node run-deterministic.mjs
import { readFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import adapter from './adapter.mjs'

const here = path.dirname(fileURLToPath(import.meta.url))
const kip = process.env.KIP_ROOT ?? path.resolve(here, '../../../KIP')
const { runMemoryVectors } = await import(pathToFileURL(path.join(kip, 'conformance/runner.mjs')))

const selected = [
  ['interface', 'KIP2-MIF-002'],
  ['interface', 'KIP2-MIF-005'],
  ['reliability', 'KIP2-REL-013']
]
const vectors = []
for (const [suite, id] of selected) {
  vectors.push(JSON.parse(await readFile(path.join(kip, 'conformance/vectors', suite, `${id}.json`), 'utf8')))
}
const report = await runMemoryVectors(adapter, vectors)
console.log(JSON.stringify(report, null, 2))
process.exit(report.overall_status === 'PASS' ? 0 : 1)
